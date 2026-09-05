import { test, expect } from '@playwright/test';

test('a Codex worker opens on Terminal, not Transcript', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');

  // Codex was the provider-specific branch that overwrote openPeek's terminal
  // reset. Seed only the dashboard's real session cache: no tmux process or LLM
  // is started, while `sessionProvider()` still takes the production Codex path.
  const tab = await page.evaluate(`
    sessions = [{ name: 'e2e-codex-default', provider: 'codex', dir: '/tmp' }];
    openPeek('e2e-codex-default');
    _peekTab;
  `);

  expect(tab).toBe('terminal');
  await expect(page.locator('#peek-tab-terminal')).toHaveClass(/active/);
  await expect(page.locator('#peek-tab-transcript')).not.toHaveClass(/active/);
  await expect(page.locator('#peek-terminal-panel')).toBeVisible();
});
