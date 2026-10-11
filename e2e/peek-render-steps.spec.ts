// Peek rendering is one named list per provider, and ⋮ → Rendering steps shows
// what ran on the frame on screen (Ethan, 2026-10-11). The peek endpoint is
// stubbed with a frame the server would send, including its `render` stamp, so
// the dashboard half runs for real against a known input.
import { test, expect, Page } from './fixtures';

const toolCallFrame = [
  '❯ fix the build',
  '',
  '⏺ Bash(cargo check)',
  '  ⎿  Finished dev profile',
  '',
  '⏺ Done. The build passes.',
].join('\n');

const workers = [
  { name: 'render-claude', provider: 'claude', model: 'sonnet', running: true, status: 'idle', dir: '/tmp' },
  { name: 'render-codex', provider: 'codex', model: 'gpt-5', running: true, status: 'idle', dir: '/tmp' },
];

async function openPeekOn(page: Page, name: string, render: { path: string; steps: string[] }) {
  await page.route(new RegExp(`/api/sessions/${name}/peek`), (r) => r.fulfill({
    headers: { ETag: `"${name}-1"` },
    json: { name, history: toolCallFrame, live: '(idle)', output: '(idle)', pane_cols: 120, render },
  }));
  await page.evaluate((n) => { (window as any).openPeek(n); }, name);
  await expect(page.locator('#peek-body')).toContainText('Done. The build passes.');
}

async function renderSteps(page: Page) {
  // Not awaited: the call resolves when the dialog closes.
  await page.evaluate(() => { (window as any)._peekShowRenderSteps(); });
  const modal = page.locator('#modal-msg');
  await expect(modal).toContainText('Rendering steps');
  // textContent, so steps folded into a closed <details> are still read.
  const text = (await modal.textContent()) || '';
  await page.evaluate(() => (window as any)._modalClose(true));
  return text;
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, (r) => r.fulfill({ json: workers }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekShowRenderSteps === 'function');
  await page.evaluate((w) => { eval('sessions=' + JSON.stringify(w) + '; render();'); }, workers);
});

test('Claude output runs the Claude pipeline: tool calls collapse, and the steps say so', async ({ page }) => {
  await openPeekOn(page, 'render-claude', { path: 'claude-transcript', steps: ['tmux-capture', 'strip-scroll-pill', 'claude-transcript', 'collapse-blank-runs'] });
  await expect(page.locator('#peek-body .ptc.collapsed')).toHaveCount(1);   // the Bash call, folded
  const text = await renderSteps(page);
  expect(text).toContain('claude (pipeline: claude)');
  expect(text).toContain('Server: history from claude-transcript');
  expect(text).toMatch(/✓\s*collapse-tool-calls/);
  // The latest poll either painted or found nothing new; both are honest outcomes.
  expect(text).toMatch(/✓\s*(painted|skip-unchanged)/);
  expect(text).toMatch(/history · full/);
  expect(text).toMatch(/Not run on the server \(\d+\)/);
});

test('Codex output runs the Codex pipeline: no Claude tool-call collapsing', async ({ page }) => {
  await openPeekOn(page, 'render-codex', { path: 'codex-rollout', steps: ['tmux-capture', 'strip-scroll-pill', 'codex-rollout', 'collapse-blank-runs'] });
  await expect(page.locator('#peek-body .ptc')).toHaveCount(0);   // the same frame, left as drawn
  const text = await renderSteps(page);
  expect(text).toContain('codex (pipeline: codex)');
  expect(text).toContain('Server: history from codex-rollout');
  expect(text).not.toMatch(/✓\s*collapse-tool-calls/);
  expect(text).toMatch(/Not in this pipeline \(\d+\)[\s\S]*·\s*collapse-tool-calls/);
  expect(text).toMatch(/✓\s*ansi-to-html/);
});

test('the menu offers Rendering steps', async ({ page }) => {
  await openPeekOn(page, 'render-claude', { path: 'tmux-scrollback', steps: ['tmux-scrollback'] });
  await page.locator('#peek-worker-menu-btn').click();
  await page.locator('#peek-render-steps-btn').click();
  await expect(page.locator('#modal-msg')).toContainText('Server: history from tmux-scrollback');
});
