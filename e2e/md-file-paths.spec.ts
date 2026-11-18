// Owner, 2026-11-18: "Make it so markdown files, file path is clickable and on
// mouseover shows a preview", and "Make sure the editor has the same kind of
// formatting stuff like tabbing to indent". A path written as plain text in a
// markdown file's Preview is a link that opens the file, and hovering it shows
// the head of that file. Tab indents in the editor for every file, not only
// markdown.
import { test, expect } from './fixtures';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const DIR = mkdtempSync(join(tmpdir(), 'amux-e2e-mdpath-'));
const OTHER = join(DIR, 'other.md');
const NOTE = join(DIR, 'todo.md');
const CODE = join(DIR, 'tool.py');
writeFileSync(OTHER, '# Other plan\n\nThe preview of the linked file.\n');
writeFileSync(NOTE, '# To do\n\n' + OTHER + '\n\nSee ' + OTHER + ', then ship.\n\nIn code: `' + OTHER + '` stays.\n');
writeFileSync(CODE, 'def f():\nreturn 1\n');

async function open(page: any, file: string) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate((p: string) => (window as any).openFilePreview(p), file);
}

test('a path in a markdown file is a link that opens the file', async ({ page }) => {
  await open(page, NOTE);
  const links = page.locator('#file-body .md-path-link');
  await expect(links).toHaveCount(2);
  await expect(links.first()).toHaveText(OTHER);
  // The comma after the second path stays text; the path inside code stays code.
  await expect(links.nth(1)).toHaveText(OTHER);
  await expect(page.locator('#file-body code')).toHaveText(OTHER);
  await links.first().click();
  await expect(page.locator('#file-body')).toContainText('The preview of the linked file.');
});

test('hovering a linked path shows the head of that file', async ({ page }) => {
  test.skip(!(await page.evaluate(() => matchMedia('(hover: hover)').matches)), 'pointers that hover only');
  await open(page, NOTE);
  await page.locator('#file-body .md-path-link').first().hover();
  const card = page.locator('#worker-hovercard');
  await expect(card).toBeVisible();
  await expect(card.locator('.file-hovercard-path')).toHaveText(OTHER);
  await expect(card).toContainText('The preview of the linked file.');
  await page.mouse.move(1, 1);
  await expect(card).toBeHidden();
});

test('Tab indents and Shift+Tab outdents in the editor for a code file', async ({ page }) => {
  await open(page, CODE);
  await page.locator('#file-tab-edit').click();
  const ta = page.locator('#file-edit-ta');
  await expect(ta).toHaveValue('def f():\nreturn 1\n');
  // Caret at the start of "return": Tab indents it, focus stays in the editor.
  await ta.evaluate((t: HTMLTextAreaElement) => { t.focus(); t.setSelectionRange(9, 9); });
  await page.keyboard.press('Tab');
  await expect(ta).toHaveValue('def f():\n  return 1\n');
  await expect(ta).toBeFocused();
  // Both lines selected: Tab indents each, Shift+Tab takes it back off.
  await ta.evaluate((t: HTMLTextAreaElement) => t.setSelectionRange(0, t.value.length));
  await page.keyboard.press('Tab');
  await expect(ta).toHaveValue('  def f():\n    return 1\n');
  await page.keyboard.press('Shift+Tab');
  await expect(ta).toHaveValue('def f():\n  return 1\n');
  // A caret inside the leading spaces outdents its line too (land review F1).
  await ta.evaluate((t: HTMLTextAreaElement) => t.setSelectionRange(10, 10));
  await page.keyboard.press('Shift+Tab');
  await expect(ta).toHaveValue('def f():\nreturn 1\n');
});
