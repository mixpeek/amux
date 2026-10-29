// Owner, 2026-10-29: "make sure edits are saved as they happen and we have
// proper collision reconcilation etc. handled for multiple users in a
// workspace". A markdown file saves itself a moment after typing stops. When
// someone else (another person, a worker) saved the file in between, the edits
// merge line by line; the same lines changed on both sides stop and ask, and
// nothing is overwritten. With nothing unsaved, their save shows up on its own.
// Real files, the real file viewer and the real save route; the "someone
// else" is a write to the file on disk, which is what a worker's edit is.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const W = 'collab-w';
const SRC = '# Plan\n\nline one\nline two\nline three\nline four\n';

function freshFile(body = SRC) {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-collab-'));
  const f = join(dir, 'plan.md');
  writeFileSync(f, body);
  return f;
}

async function open(page: Page, f: string) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: '/tmp' }] }));
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_mdx_mode', 'edit'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#mdx')).toBeVisible();
  await page.locator('#mdx-mode [data-m="edit"]').click();
}

// Type `text` at the start of line `n` (0-based) of the source.
async function typeAtLine(page: Page, n: number, text: string) {
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement, n) => {
    const L = el.value.split('\n'); let off = 0;
    for (let k = 0; k < n; k++) off += L[k].length + 1;
    el.focus(); el.setSelectionRange(off, off);
  }, n);
  await page.keyboard.type(text);
}

test('an edit saves itself a moment after typing stops', async ({ page }) => {
  const f = freshFile();
  await open(page, f);
  await typeAtLine(page, 2, 'NEW ');
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 8000 }).toContain('NEW line one');
  await expect(page.locator('#mdx-dirty')).toHaveText('Saved');
});

test('edits to different lines merge with a save made elsewhere', async ({ page }) => {
  const f = freshFile();
  await open(page, f);
  // Someone else saves first, on line four.
  writeFileSync(f, SRC.replace('line four', 'line four (theirs)'));
  await typeAtLine(page, 2, 'MINE ');
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 10000 }).toBe(SRC.replace('line one', 'MINE line one').replace('line four', 'line four (theirs)'));
  await expect(page.locator('#file-edit-ta')).toHaveValue(/MINE line one[\s\S]*line four \(theirs\)/);
  await expect(page.locator('#file-conflict')).toHaveCount(0);
});

test('the same line changed on both sides keeps both and asks', async ({ page }) => {
  const f = freshFile();
  await open(page, f);
  writeFileSync(f, SRC.replace('line one', 'THEIRS line one'));
  await typeAtLine(page, 2, 'MINE ');
  const bar = page.locator('#file-conflict');
  await expect(bar).toBeVisible({ timeout: 10000 });
  // Nothing was overwritten: theirs on disk, mine in the editor.
  expect(readFileSync(f, 'utf8')).toContain('THEIRS line one');
  await expect(page.locator('#file-edit-ta')).toHaveValue(/MINE line one/);
  // Their version is one click away.
  await bar.getByRole('button', { name: 'Show their version' }).click();
  await expect(page.locator('.mdx-diff-head')).toContainText('their version');
  await expect(page.locator('.mdx-row-del')).toContainText('THEIRS line one');
  // Keep mine: saved over theirs, on purpose.
  await bar.getByRole('button', { name: 'Keep mine' }).click();
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 8000 }).toContain('MINE line one');
  await expect(bar).toBeHidden();
  await page.screenshot({ path: test.info().outputPath('md-collab-kept.png') });
});

test('Take theirs replaces the edit with the other version', async ({ page }) => {
  const f = freshFile();
  await open(page, f);
  writeFileSync(f, SRC.replace('line one', 'THEIRS line one'));
  await typeAtLine(page, 2, 'MINE ');
  const bar = page.locator('#file-conflict');
  await expect(bar).toBeVisible({ timeout: 10000 });
  await page.screenshot({ path: test.info().outputPath('md-collab-conflict.png') });
  await bar.getByRole('button', { name: 'Take theirs' }).click();
  await expect(page.locator('#file-edit-ta')).toHaveValue(/THEIRS line one/);
  await expect(page.locator('#file-edit-ta')).not.toHaveValue(/MINE/);
  await expect(bar).toBeHidden();
  expect(readFileSync(f, 'utf8')).toContain('THEIRS line one');
});

test('with nothing unsaved, a save made elsewhere shows up on its own', async ({ page }) => {
  const f = freshFile();
  await open(page, f);
  await page.locator('#mdx-mode [data-m="preview"]').click();
  writeFileSync(f, SRC.replace('line two', 'line two, edited by a worker'));
  await expect(page.locator('#file-body')).toContainText('edited by a worker', { timeout: 10000 });
  await expect(page.locator('#file-edit-ta')).toHaveValue(/edited by a worker/);
});

test('two saves in the same second with the same length are told apart', async ({ page }) => {
  // mtime and size alone match here; the etag's content hash does not.
  const f = freshFile();
  await open(page, f);
  writeFileSync(f, SRC.replace('line one', 'LINE ONE'));
  await typeAtLine(page, 2, 'x');
  await expect(page.locator('#file-conflict')).toBeVisible({ timeout: 10000 });
  expect(readFileSync(f, 'utf8')).toContain('LINE ONE');
});
