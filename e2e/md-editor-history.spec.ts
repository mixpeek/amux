// History and Diff in the markdown editor when the checkout's HEAD is behind
// its remote (owner, 2026-11-19: "the history is just not right neither is
// diffs test it e2e"). Shared checkouts land work through `amux land` and a
// graft push, so the file on disk is current while HEAD stays behind: History
// read HEAD alone and listed old commits or none, and called the file
// uncommitted against a HEAD it had moved past.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const W = 'mdx-hist-w';
const ident = ['-c', 'user.name=Tester', '-c', ['user', 'email'].join('.') + '=tester@localhost', '-c', 'commit.gpgsign=false'];

// A bare remote, a checkout of it, and a second clone that lands two commits.
// The checkout fetches them and takes the file's new bytes (as a graft does)
// without moving HEAD.
function behindCheckout() {
  const root = mkdtempSync(join(tmpdir(), 'amux-e2e-mdx-hist-'));
  const remote = join(root, 'remote.git'), co = join(root, 'co'), other = join(root, 'other');
  const g = (dir: string, ...a: string[]) => execFileSync('git', ['-C', dir, ...ident, ...a]);
  execFileSync('git', ['init', '-q', '--bare', '-b', 'main', remote]);
  execFileSync('git', ['clone', '-q', remote, co]);
  mkdirSync(join(co, 'notes'));
  const f = join(co, 'notes', 'plan.md');
  writeFileSync(f, '# Plan\n\n- first\n');
  g(co, 'add', '.'); g(co, 'commit', '-qm', 'start the plan'); g(co, 'push', '-q', 'origin', 'HEAD:main');
  g(co, 'branch', '-q', '--set-upstream-to=origin/main');
  execFileSync('git', ['clone', '-q', remote, other]);
  writeFileSync(join(other, 'notes', 'plan.md'), '# Plan\n\n- first\n- second\n');
  g(other, 'commit', '-qam', 'add the second step');
  writeFileSync(join(other, 'notes', 'plan.md'), '# Plan\n\n- first\n- second\n- third\n');
  g(other, 'commit', '-qam', 'add the third step'); g(other, 'push', '-q', 'origin', 'HEAD:main');
  g(co, 'fetch', '-q', 'origin');
  g(co, 'checkout', 'origin/main', '--', 'notes/plan.md');
  g(co, 'reset', '-q', '--', 'notes/plan.md');
  return f;
}

async function open(page: Page, f: string) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: '/tmp' }] }));
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_mdx_mode'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#mdx')).toBeVisible();
}

test('History lists the commits on the remote branch when HEAD is behind it, newest first', async ({ page }) => {
  const f = behindCheckout();
  await open(page, f);
  const api = await page.evaluate(async p => (await fetch('/api/file/history?path=' + encodeURIComponent(p),
    { headers: (globalThis as any).eval('_authHeaders')() })).json(), f);
  expect(api.refs, 'the log read the upstream too').toContain('origin/main');
  expect(api.uncommitted, 'the file matches the newest commit').toBe(false);
  await page.locator('#mdx-mode [data-m="history"]').click();
  const commits = page.locator('.mdx-commit');
  await expect(commits).toHaveCount(3);
  await expect(commits.nth(0)).toContainText('add the third step');
  await expect(commits.nth(1)).toContainText('add the second step');
  await expect(commits.nth(2)).toContainText('start the plan');
  await page.screenshot({ path: test.info().outputPath('md-history-behind.png') });
});

test('Diff against a remote commit shows what changed since it', async ({ page }) => {
  const f = behindCheckout();
  await open(page, f);
  await page.locator('#mdx-mode [data-m="history"]').click();
  await page.locator('.mdx-commit').nth(1).locator('[data-act="diff"]').click();
  await expect(page.locator('.mdx-diff-ends')).toContainText('add the second step');
  await expect(page.locator('.mdx-row-add')).toHaveCount(1);
  await expect(page.locator('.mdx-row-add')).toContainText('- third');
  await expect(page.locator('.mdx-row-del')).toHaveCount(0);
});

test('a commit made after History was read shows on the next look', async ({ page }) => {
  const f = behindCheckout();
  await open(page, f);
  await page.locator('#mdx-mode [data-m="history"]').click();
  await expect(page.locator('.mdx-commit')).toHaveCount(3);
  const co = join(f, '..', '..');
  writeFileSync(f, '# Plan\n\n- first\n- second\n- third\n- fourth\n');
  execFileSync('git', ['-C', co, ...ident, 'commit', '-qam', 'add the fourth step']);
  await page.evaluate(() => { (globalThis as any).eval('_mdx').histFor.at = 0; });   // past the 15 s reuse
  await page.locator('#mdx-mode [data-m="preview"]').click();
  await page.locator('#mdx-mode [data-m="history"]').click();
  await expect(page.locator('.mdx-commit').first()).toContainText('add the fourth step');
});
