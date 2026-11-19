// The conversations from Intent routed to a worker live in its own Intent tab
// (owner, 2026-11-19: "These should be in a separate intent tab in the worker
// details page"), not above its Messages list. The tab shows only for a worker
// that has some, with their count, so other workers' tab rows do not grow.
import { test, expect, Page } from './fixtures';

const WITH = 'zz-intent-has';
const WITHOUT = 'zz-intent-none';

async function boot(page: Page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: WITH, dir: '/tmp/intent-has', running: true, status: 'idle', flags: '', steering: [] },
    { name: WITHOUT, dir: '/tmp/intent-none', running: true, status: 'idle', flags: '', steering: [] }] }));
  await page.route(/\/api\/intent\/outcomes(?:\?.*)?$/, r => r.fulfill({ json: { outcomes: [
    { id: 'oc-1', worker: WITH, title: 'Keep every CI run under $1', status: 'ready', updated_ms: Date.now() },
    { id: 'oc-2', worker: WITH, title: 'Get GS-12 proof runs done', status: 'working', updated_ms: Date.now() }] } }));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any).setPeekTab === 'function');
}

test('a worker with Intent conversations has an Intent tab listing them, and Messages has none', async ({ page }, info) => {
  await boot(page);
  await page.evaluate((w) => (window as any).openPeek(w), WITH);
  const tab = page.locator('#peek-tab-intent');
  await expect(tab).toBeVisible();
  await expect(tab.locator('.peek-tab-count')).toHaveText('2');
  await page.evaluate(() => (window as any).setPeekTab('messages'));
  await expect(page.locator('#peek-messages-panel')).not.toContainText('Keep every CI run under $1');
  await tab.click();
  const panel = page.locator('#peek-intent-panel');
  await expect(panel).toBeVisible();
  await expect(panel).toContainText('Keep every CI run under $1');
  await expect(panel).toContainText('Get GS-12 proof runs done');
  // The tab joins the one tab row: same row as Terminal, inside the screen.
  const term = (await page.locator('#peek-tab-terminal').boundingBox())!;
  const t = (await tab.boundingBox())!;
  expect(Math.abs((t.y + t.height / 2) - (term.y + term.height / 2)), 'the Intent tab is on the tab row').toBeLessThan(t.height);
  await page.screenshot({ path: info.outputPath('peek-intent-tab.png') });
});

test('a worker with no Intent conversations shows no Intent tab', async ({ page }) => {
  await boot(page);
  await page.evaluate((w) => (window as any).openPeek(w), WITH);
  await expect(page.locator('#peek-tab-intent')).toBeVisible();
  await page.evaluate(() => (window as any).closePeek());
  await page.evaluate((w) => (window as any).openPeek(w), WITHOUT);
  await expect(page.locator('#peek-tab-intent')).toBeHidden();
});
