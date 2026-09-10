import {test, expect} from './fixtures';

// AMUX-4366: use actual settings tabs, without the endpoint suite's CSS override.
test('settings_native_context_management', async ({page}, info) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.locator('#settings-btn').click();
  await page.locator('.settings-tab-btn[data-stab="workers"]').click();
  await expect(page.locator('#native-compaction-info')).toBeVisible();
  await expect(page.locator('#stab-workers')).toContainText('Claude Code compacts context and continues the task automatically.');
  await expect(page.locator('#stab-workers')).toContainText('/config menu');
  await expect(page.locator('#auto-compact-checkbox')).toHaveCount(0);
  await page.screenshot({path:info.outputPath('native-context-management.png')});
});
