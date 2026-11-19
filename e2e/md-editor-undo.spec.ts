// Undo and Redo in the markdown editor, by button and by key (owner,
// 2026-11-19: "Undo / redo doesnt work in the markdown editor - fix it. The
// button should works and the shortcuts"). Typed text, a formatting tool, and
// typing that has already autosaved, which is the usual case.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const W = 'mdx-undo-w';

function freshFile(body = 'hello world\n') {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-mdundo-'));
  const f = join(dir, 'undo.md');
  writeFileSync(f, body);
  return f;
}

async function openEdit(page: Page, f: string) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: '/tmp' }] }));
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_mdx_mode'); });
  await page.goto('/');
  await page.waitForFunction(w => typeof (window as any).openFilePreview === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#mdx')).toBeVisible();
  await page.locator('#mdx-mode [data-m="edit"]').click();
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(el.value.length, el.value.length); });
  return ta;
}
const undoBtn = (page: Page) => page.locator('#mdx-tools [data-t="undo"]');
const redoBtn = (page: Page) => page.locator('#mdx-tools [data-t="redo"]');

test('the Undo and Redo buttons take back typing and put it back', async ({ page }) => {
  const ta = await openEdit(page, freshFile());
  await page.keyboard.type('more');
  await expect(ta).toHaveValue('hello world\nmore');
  await undoBtn(page).click();
  await expect(ta).toHaveValue('hello world\n');
  await redoBtn(page).click();
  await expect(ta).toHaveValue('hello world\nmore');
});

test('Undo works after the edit has autosaved, and the undone text saves too', async ({ page }) => {
  const f = freshFile();
  const ta = await openEdit(page, f);
  await page.keyboard.type('saved');
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 10000 }).toBe('hello world\nsaved');
  await expect(page.locator('#mdx-dirty')).toHaveText('Saved');
  await undoBtn(page).click();
  await expect(ta).toHaveValue('hello world\n');
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 10000 }).toBe('hello world\n');
  await page.keyboard.press('ControlOrMeta+Shift+z');
  await expect(ta).toHaveValue('hello world\nsaved');
});

test('⌘Z and ⇧⌘Z undo and redo a formatting tool', async ({ page }) => {
  const ta = await openEdit(page, freshFile());
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(6, 11); });
  await page.locator('#mdx-tools [data-t="bold"]').click();
  await expect(ta).toHaveValue('hello **world**\n');
  await page.keyboard.press('ControlOrMeta+z');
  await expect(ta).toHaveValue('hello world\n');
  await page.keyboard.press('ControlOrMeta+Shift+z');
  await expect(ta).toHaveValue('hello **world**\n');
});

test('Undo still works after the file is re-read from disk with nothing typed', async ({ page }) => {
  const f = freshFile();
  const ta = await openEdit(page, f);
  await page.keyboard.type('one');
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 10000 }).toBe('hello world\none');
  // The editor follows the file every few seconds while nothing is unsaved.
  await page.waitForTimeout(5000);
  await page.keyboard.type('two');
  await undoBtn(page).click();
  await expect(ta).toHaveValue('hello world\none');
});

test('the Mac app\'s Edit menu (historyUndo / historyRedo) steps the same history', async ({ page }) => {
  const ta = await openEdit(page, freshFile());
  await page.keyboard.type('menu');
  const fire = (t: string) => ta.evaluate((el, t) => el.dispatchEvent(new InputEvent('beforeinput', { inputType: t, bubbles: true, cancelable: true })), t);
  await fire('historyUndo');
  await expect(ta).toHaveValue('hello world\n');
  await fire('historyRedo');
  await expect(ta).toHaveValue('hello world\nmenu');
});

test('typing groups into steps: one Undo takes back the last word, not one letter', async ({ page }) => {
  const ta = await openEdit(page, freshFile(''));
  await page.keyboard.type('first second');
  await undoBtn(page).click();
  await expect(ta).toHaveValue('first ');
  await undoBtn(page).click();
  await expect(ta).toHaveValue('');
  await redoBtn(page).click();
  await redoBtn(page).click();
  await expect(ta).toHaveValue('first second');
});
