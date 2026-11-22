// Your picture button in the Messages sidebar (#cv-me) is empty until
// /api/profiles answers and _cvRenderMe fills it. With no size of its own it
// measured 0 wide in that window: the header jumped when the picture arrived,
// and messages-sidebar-fold read "#cv-me has a size: 0" on a slow nightly run
// (AF-1344). The button keeps the picture's 26px box while it is empty.
import { test, expect } from './fixtures';

test('the picture button keeps its size before the picture arrives', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate(() => { document.getElementById('cv-me')!.innerHTML = ''; });
  const b = (await page.locator('#cv-me').boundingBox())!;
  expect(b.width, 'an empty #cv-me still has the picture\'s width').toBeGreaterThan(0);
});
