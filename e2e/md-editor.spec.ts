// The markdown editor (owner, 2026-10-29: "redesign the md file viewer/editor
// around this. maintain our functionality but put it in this format and add
// to it", with a mockup: Edit / Preview / Split and a formatting bar over a
// numbered source; Preview / Diff / History beside it; a status line).
// Real files on disk, the real file viewer, the real save; History reads a
// real git repository through GET /api/file/history.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync, readFileSync, mkdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const W = 'mdx-w';
const SRC = '# To-Do\n\n## 2026-10-12\n\n- [ ] #gtm MomentBench wrap up\n- [x] #ops finances health check\n- ask @' + W + ' about it\n\nSee https://example.com/docs\n';

function freshFile(name = 'todo.md', body = SRC) {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-mdx-'));
  const f = join(dir, name);
  writeFileSync(f, body);
  return f;
}

async function open(page: Page, f: string) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: '/tmp' }] }));
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_mdx_mode'); });
  await page.goto('/');
  await page.waitForFunction(w => typeof (window as any).openFilePreview === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#mdx')).toBeVisible();
}
const wide = (page: Page) => (page.viewportSize()?.width || 1280) > 700;
const mode = (page: Page, m: string) => page.locator('#mdx-mode [data-m="' + m + '"]').click();

test('a markdown file opens in Preview, with tags, worker links, tasks and heading links', async ({ page }) => {
  await open(page, freshFile());
  await expect(page.locator('#mdx')).toHaveAttribute('data-mode', 'preview');
  const out = page.locator('#mdx-out-host #file-body');
  await expect(out.locator('h1')).toHaveText(/To-Do/);
  await expect(out.locator('.md-tag')).toHaveText(['#gtm', '#ops']);
  await expect(out.locator('.md-worker-link')).toHaveText('@' + W);
  await expect(out.locator('input[type="checkbox"][data-task]')).toHaveCount(2);
  await expect(out.locator('h2 .mdx-anchor')).toHaveCount(1);
  // The viewer's own Preview/Raw/Edit tabs give way to the editor's.
  await expect(page.locator('#file-view-tabs')).toBeHidden();
  await page.screenshot({ path: test.info().outputPath('md-preview.png') });
});

test('Split: the source with line numbers beside a live preview, and a status line', async ({ page }) => {
  test.skip(!wide(page), 'Split needs a wide screen; a phone has Edit and Preview');
  await open(page, freshFile());
  await mode(page, 'split');
  await expect(page.locator('.mdx-src')).toBeVisible();
  await expect(page.locator('.mdx-view')).toBeVisible();
  await expect(page.locator('#mdx-hl .ln')).toHaveCount(SRC.split('\n').length);
  await expect(page.locator('#mdx-hl .ln.mdx-h').first()).toHaveAttribute('data-n', '1');
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(el.value.length, el.value.length); });
  await page.keyboard.type('\n## Added live\n');
  await expect(page.locator('#file-body h2', { hasText: 'Added live' })).toBeVisible();
  await expect(page.locator('#mdx-dirty')).toHaveText('Unsaved');
  await expect(page.locator('#mdx-pos')).toHaveText(/^Ln \d+, Col 1$/);
  await expect(page.locator('#mdx-chars')).toContainText('characters');
  await page.screenshot({ path: test.info().outputPath('md-split.png') });
});

test('the formatting bar and keys edit the source, and ⌘Z undoes them', async ({ page }) => {
  const f = freshFile('fmt.md', 'hello world\n');
  await open(page, f);
  await mode(page, 'edit');
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(6, 11); });
  await page.locator('#mdx-tools [data-t="bold"]').click();
  await expect(ta).toHaveValue('hello **world**\n');
  await page.locator('#mdx-tools [data-t="heading"]').click();
  await expect(ta).toHaveValue('# hello **world**\n');
  await page.keyboard.press('ControlOrMeta+z');
  await expect(ta).toHaveValue('hello **world**\n');
  // A task, then Enter carries the list on, and Enter on an empty item ends it.
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(el.value.length, el.value.length); });
  await page.keyboard.type('ship it');
  await page.locator('#mdx-tools [data-t="task"]').click();
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(el.value.length, el.value.length); });
  await page.keyboard.press('Enter');
  await page.keyboard.type('test it');
  await page.keyboard.press('Enter');
  await page.keyboard.press('Enter');
  await expect(ta).toHaveValue('hello **world**\n- [ ] ship it\n- [ ] test it\n');
  // Save writes the file.
  await page.keyboard.press('ControlOrMeta+s');
  await expect.poll(() => readFileSync(f, 'utf8')).toBe('hello **world**\n- [ ] ship it\n- [ ] test it\n');
  await expect(page.locator('#mdx-dirty')).toHaveText('Saved');
});

test('ticking a task in Preview ticks it in the file', async ({ page }) => {
  const f = freshFile();
  await open(page, f);
  await page.locator('#file-body input[data-task="0"]').check();
  await expect.poll(() => readFileSync(f, 'utf8')).toContain('- [x] #gtm MomentBench wrap up');
  await page.locator('#file-body input[data-task="1"]').uncheck();
  await expect.poll(() => readFileSync(f, 'utf8')).toContain('- [ ] #ops finances health check');
});

test('Diff shows what changed against the saved file', async ({ page }) => {
  await open(page, freshFile());
  await mode(page, 'edit');
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(0, 7); });
  await page.keyboard.type('# Done');
  await mode(page, wide(page) ? 'split' : 'preview');
  await page.locator('#mdx-tab [data-v="diff"]').click();
  await expect(page.locator('.mdx-diff-head')).toContainText('+1');
  await expect(page.locator('.mdx-diff-head')).toContainText('−1');
  await expect(page.locator('.mdx-row-del')).toContainText('# To-Do');
  await expect(page.locator('.mdx-row-add')).toContainText('# Done');
});

test('History lists the commits; Diff against one; Restore puts it back unsaved', async ({ page }) => {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-mdx-git-'));
  mkdirSync(join(dir, 'notes'));
  const f = join(dir, 'notes', 'log.md');
  const ident = ['-c', 'user.name=Tester', '-c', ['user', 'email'].join('.') + '=tester@localhost', '-c', 'commit.gpgsign=false'];
  const git = (...a: string[]) => execFileSync('git', ['-C', dir, ...ident, ...a]);
  git('init', '-q');
  writeFileSync(f, '# Log\n\n- first\n'); git('add', '.'); git('commit', '-qm', 'start the log');
  writeFileSync(f, '# Log\n\n- first\n- second\n'); git('commit', '-qam', 'add the second');
  await open(page, f);
  await page.locator('#mdx-tab [data-v="history"]').click();
  const commits = page.locator('.mdx-commit');
  await expect(commits).toHaveCount(2);
  await expect(commits.nth(0)).toContainText('add the second');
  await expect(commits.nth(1)).toContainText('start the log');
  await commits.nth(1).locator('[data-act="diff"]').click();
  await expect(page.locator('.mdx-diff-head')).toContainText('start the log');
  await expect(page.locator('.mdx-row-add')).toContainText('- second');
  await page.locator('#mdx-tab [data-v="history"]').click();
  await page.locator('.mdx-commit').nth(1).locator('[data-act="restore"]').click();
  await expect(page.locator('#file-edit-ta')).toHaveValue('# Log\n\n- first\n');
  await expect(page.locator('#mdx-dirty')).toHaveText('Unsaved');
  expect(readFileSync(f, 'utf8')).toBe('# Log\n\n- first\n- second\n');
});

test('a file outside git says it has no history', async ({ page }) => {
  await open(page, freshFile());
  await page.locator('#mdx-tab [data-v="history"]').click();
  await expect(page.locator('#mdx-alt .mdx-empty')).toContainText('not in a git repository');
});

test('unsaved edits are guarded on close even from Preview', async ({ page }) => {
  await open(page, freshFile());
  await mode(page, 'edit');
  await page.locator('#file-edit-ta').evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(0, 0); });
  await page.keyboard.type('x');
  await mode(page, 'preview');
  expect(await page.evaluate(() => (globalThis as any).eval('_fileEditDirty()'))).toBe(true);
  await page.evaluate(() => (window as any).closeFilePreview());
  await expect(page.locator('#file-overlay')).toHaveClass(/active/);
});

test('any other file keeps the plain viewer, and a phone shows one pane with one row of tools', async ({ page }) => {
  const txt = freshFile('notes.txt', 'plain text\n');
  const md = freshFile();
  await open(page, md);
  await page.evaluate(p => (window as any).openFilePreview(p), txt);
  await expect(page.locator('#mdx')).toBeHidden();
  await expect(page.locator('#file-body')).toContainText('plain text');
  await page.evaluate(() => (window as any).setFileViewMode('edit'));
  await expect(page.locator('#file-edit-ta')).toHaveValue('plain text\n');
  if (wide(page)) return;
  await page.evaluate(() => (window as any).closeFilePreview());
  await page.evaluate(p => (window as any).openFilePreview(p), md);
  await expect(page.locator('#mdx [data-m="split"]')).toBeHidden();
  await mode(page, 'edit');
  await expect(page.locator('.mdx-view')).toBeHidden();
  const bar = await page.locator('#mdx-src-bar').boundingBox();
  expect(bar!.height).toBeLessThanOrEqual(60);
  await page.screenshot({ path: test.info().outputPath('md-phone-edit.png') });
});

// Land review F2: a tick made beside the source keeps the undo history.
test('a task ticked in Split can be undone with ⌘Z', async ({ page }) => {
  test.skip(!wide(page), 'Split needs a wide screen');
  await open(page, freshFile());
  await mode(page, 'split');
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(el.value.length, el.value.length); });
  await page.keyboard.type('\nmore');
  await page.locator('#file-body input[data-task="0"]').check();
  await expect(ta).toHaveValue(/- \[x\] #gtm MomentBench/);
  await ta.focus();
  await page.keyboard.press('ControlOrMeta+z');
  await expect(ta).toHaveValue(/- \[ \] #gtm MomentBench/);
  await expect(ta).toHaveValue(/\nmore$/);
});
