import { test, expect } from '../fixtures';
import { boot, auth, checkpoint } from './evidence';

test('LC-BOARD: create through UI, inspect, reload, search and export exact new work', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const pageErrors: string[] = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  await boot(page);
  const headers = await auth(page);
  const title = `lifecycle-${info.project.name}-${Date.now()} — preserve this complete task title when switching between desktop and phone layouts`;
  await page.locator('#tab-board').click();
  await page.locator('.board-new-btn').click();
  await page.locator('#be-title').fill(title);
  await page.locator('#be-desc').fill('Acceptance: preserve this exact note across reload and export.');
  await checkpoint(page, info, '01-new-task-form');
  await page.locator('.be-save').click();
  await expect(page.locator('#board-edit-overlay')).not.toHaveClass(/active/);
  let card: any;
  await expect.poll(async () => {
    const response = await request.get('/api/board?done_limit=0', { headers });
    expect(response.ok()).toBeTruthy();
    const matches = (await response.json()).filter((row: any) => row.title === title);
    card = matches[0];
    return matches.length;
  }).toBe(1);
  // Read-only oracle. This test never manufactures task completion by PATCH.
  await page.goto(`/#issue=${encodeURIComponent(card.id)}`);
  await expect(page.locator('#bd-key')).toHaveText(card.id);
  await expect(page.locator('#bd-preview')).toContainText('preserve this exact note');
  const initialViewport = page.viewportSize()!;
  for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
    await page.setViewportSize(size);
    await expect.poll(() => page.locator('#bd-title').evaluate(el => el.scrollHeight <= el.clientHeight + 1),
      { message: 'the saved title must remain fully readable after resizing' }).toBe(true);
    await checkpoint(page, info, `02-title-wrap-${size.width}`);
  }
  await page.setViewportSize(initialViewport);
  expect(pageErrors.filter(error => /ResizeObserver/.test(error))).toEqual([]);
  await checkpoint(page, info, '02-persisted-task-detail');
  await page.reload();
  await expect(page.locator('#bd-key')).toHaveText(card.id);
  await expect(page.locator('#bd-preview')).toContainText('preserve this exact note');
  await page.locator('#board-detail-overlay > .overlay-header').getByRole('button', { name: /Back/ }).click();
  await page.locator('#tab-board').click();
  await page.locator('#board-search').fill(title);
  await expect(page.locator('#board-columns')).toContainText(title);
  await checkpoint(page, info, '03-search-result');
  const download = page.waitForEvent('download');
  await page.locator('#board-export').getByRole('button', { name: 'JSON', exact: true }).click();
  const file = await download;
  const destination = info.outputPath('board-export.json');
  await file.saveAs(destination);
  const fs = await import('node:fs/promises');
  const exported = await fs.readFile(destination, 'utf8');
  expect(exported).toContain(title);
  expect(exported).toContain(card.id);
  await info.attach('exported-work', { path: destination, contentType: 'application/json' });
  expect(pageErrors.filter(error => /ResizeObserver/.test(error))).toEqual([]);
});

// Enumerated product surfaces, not a BFS that blindly starts workers or sends email.
// Each runs independently so one broken panel does not hide all later panels.
for (const surface of ['sessions', 'board', 'groups', 'calendar', 'scheduler', 'files',
  'mdai', 'proxies', 'email', 'connectors', 'logs', 'messages', 'skills', 'sql',
  'map', 'metrics', 'cost', 'torrents', 'terminal', 'browser']) {
  test(`LC-VIEW: ${surface} opens with an inspectable control inventory`, async ({ page }, info) => {
    await boot(page);
    const tab = page.locator(`#tab-${surface}`);
    // Hidden custom tabs must be enabled using the customizer, not DOM mutation.
    if (!await tab.isVisible()) {
      await page.locator('.tab-customize-wrap > .tab-customize-btn').click();
      const row = page.locator('#tab-customizer-menu').locator(`[data-tab-id="${surface}"] input[type="checkbox"]`);
      await expect(row, `customizer must expose ${surface}`).toBeVisible();
      await row.check();
      await page.locator('.tab-customize-wrap > .tab-customize-btn').click();
    }
    await tab.scrollIntoViewIfNeeded();
    await tab.click();
    await expect(page.locator(surface === 'sessions' ? '#session-view' : `#${surface}-view`)).toBeVisible();
    await checkpoint(page, info, `view-${surface}`);
  });
}
