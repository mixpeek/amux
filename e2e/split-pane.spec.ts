// Split is a new pane with a tab of its own (owner, 2026-10-29: "lets
// deprecate split view entirely, i like the split button but i onyl like it
// because it creates a split pane with aen entirely new tab. that should take
// the place of any split view. light when i right click a file"). In the Mac
// app every split asks its pane bridge (webkit.messageHandlers.amuxPane) for a
// new pane; here a stand-in bridge records what it is asked. The in-page split
// never opens, even for a worker that had it open before.
import { test, expect, Page } from './fixtures';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const DIR = mkdtempSync(join(tmpdir(), 'amux-e2e-splitpane-'));
const FILE = join(DIR, 'plan.md');
writeFileSync(FILE, '# Plan\n');
const W = 'split-pane-w';

async function boot(page: Page, bridge: boolean) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: DIR }] }));
  await page.route(/\/api\/sessions\/split-pane-w\/peek\?/, r => r.fulfill({ json: { name: W, live: 'see ' + FILE + ' for it\n', history: '' } }));
  await page.route(/\/api\/sessions\/split-pane-w\/subagents$/, r => r.fulfill({ json: { session: W, subagents: [] } }));
  await page.addInitScript(([on, w]) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    // A worker that had the old split open, Files beside the terminal.
    localStorage.setItem('amux_peek_views', JSON.stringify({ [w as string]: { tab: 'terminal', split: true, splitView: 'files' } }));
    (window as any).__splits = [];
    if (on) (window as any).webkit = { messageHandlers: { amuxPane: { postMessage: (m: any) => (window as any).__splits.push(m) } } };
  }, [bridge, W]);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(n => (window as any).openPeek(n), W);
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
}
const splits = (page: Page) => page.evaluate(() => (window as any).__splits as any[]);

test('in the Mac app, Split opens this page again in a new pane', async ({ page }) => {
  await boot(page, true);
  const btn = page.locator('#peek-split-toggle');
  await expect(btn).toBeVisible();
  await btn.click();
  const s = await splits(page);
  expect(s.length).toBe(1);
  expect(s[0].op).toBe('split');
  expect(new URL(s[0].url).hash).toContain('peek=' + W);
  await expect(page.locator('#peek-split-wrap')).not.toHaveClass(/split-active/);
});

test('in the Mac app, a file opens in a new pane from its right-click menu', async ({ page }) => {
  await boot(page, true);
  const n = await page.evaluate(p => {
    const body = document.getElementById('peek-body')!;
    body.innerHTML = (window as any)._peekHtml('see ' + p + ' for it\n');
    const link = body.querySelector('.file-link, .md-link') as HTMLElement;
    const r = link.getBoundingClientRect();
    link.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.left + 4, clientY: r.top + 4 }));
    return 1;
  }, FILE);
  expect(n).toBe(1);
  await page.locator('#file-ctx-menu .card-menu-item', { hasText: 'Open in split pane' }).click();
  await expect.poll(() => splits(page).then(s => s.length)).toBe(1);
  const s = await splits(page);
  expect(decodeURIComponent(new URL(s[0].url).hash)).toBe('#path=' + FILE);
  await expect(page.locator('#peek-split-wrap')).not.toHaveClass(/split-active/);
});

test('in the Mac app, File browser opens the worker folder in a new pane', async ({ page }) => {
  await boot(page, true);
  await page.evaluate(() => (window as any)._peekFileBrowser());
  const s = await splits(page);
  expect(s.length).toBe(1);
  expect(decodeURIComponent(new URL(s[0].url).hash)).toBe('#path=' + DIR);
});

test('a browser shows no Split button and never opens the old split, even a remembered one', async ({ page }) => {
  await boot(page, false);
  await expect(page.locator('#peek-split-toggle')).toBeHidden();
  await page.waitForTimeout(600);
  await expect(page.locator('#peek-split-wrap')).not.toHaveClass(/split-active/);
  // The old entry points do nothing to the page's own layout.
  await page.evaluate(() => (window as any)._peekSplitOpen('files'));
  await expect(page.locator('#peek-split-wrap')).not.toHaveClass(/split-active/);
  const said = await page.evaluate(async () => {
    const r = await (await fetch('/api/client-debug?kind=split')).json();
    return (r.beacons || []).some((b: any) => b.body && b.body.verdict === 'split_view_retired');
  });
  expect(said, 'the refusal is beaconed').toBe(true);
});
