// Ethan, 2026-10-20: "hide the magnifying glass at the top". Search stays
// reachable without a keyboard, from the + menu.
import {test, expect} from './fixtures';

test('the header magnifying glass is hidden and the + menu opens search', async ({page}) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await expect(page.locator('#add-btn')).toBeVisible();
  await expect(page.locator('#cmdk-btn')).toBeHidden();
  await page.locator('#add-btn').click();
  await page.locator('#add-menu-search').click();
  await expect(page.locator('#cmdk-overlay')).toHaveClass(/active/);
});
