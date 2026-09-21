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

test('terminal identity uses canonical model and active worktree while fan-out eligibility stays authoritative', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(`
    sessions = [
      {name:'ollama-parent',provider:'ollama',model:'qwen3-coder:30b-65k',active_model:'',dir:'/repo',worktree_active:true,worktree_path:'/tmp/exact-worktree'},
      {name:'ollama-child',provider:'ollama',model:'qwen3-coder:30b-65k',dir:'/repo',ephemeral:true,ephemeral_parent:'ollama-parent'}
    ];
    openPeek('ollama-parent');
  `);
  await expect(page.locator('#peek-model-badge')).toContainText('qwen3-coder:30b-65k');
  await expect(page.locator('#peek-dir-text')).toHaveText('/tmp/exact-worktree');
  await expect(page.locator('#peek-tab-fanout')).toBeVisible();
  await page.evaluate(`peekHiddenTabs.delete('fanout');_applyPeekTabVisibility()`);
  await expect(page.locator('#peek-tab-fanout')).toBeVisible();

  await page.evaluate(`closePeek()`);
  await expect(page.locator('#peek-overlay')).toHaveAttribute('aria-hidden','true');
  await expect(page.locator('#peek-overlay')).toHaveAttribute('inert','');
  await expect(page.locator('#peek-overlay')).toBeHidden();

  await page.evaluate(`openPeek('ollama-child')`);
  await expect(page.locator('#peek-tab-fanout')).toBeHidden();
  await page.evaluate(`peekHiddenTabs.delete('fanout');_applyPeekTabVisibility()`);
  await expect(page.locator('#peek-tab-fanout')).toBeHidden();
});
