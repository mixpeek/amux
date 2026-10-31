// Owner, 2026-10-31: "improve the markdown editor on mobile view, make it snappier, faster to
// load, faster to edit, better ux, i noticed its hard to edit". On a phone the source is the
// plain textarea at 16px, one layer, so text, caret and selection line up; formatting buttons
// keep the keyboard; and a keystroke rewrites only the line it changed.
import { test, expect, Page } from './fixtures';
import { mkdtempSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

function note(lines = 300) {
  const L = ['# Notes', ''];
  for (let i = 0; i < lines; i++) L.push('## Section ' + i, '', '- [ ] task #tag with **bold**', 'A paragraph line ' + i, '');
  const f = join(mkdtempSync(join(tmpdir(), 'amux-e2e-mdm-')), 'notes.md');
  writeFileSync(f, L.join('\n'));
  return f;
}
async function open(page: Page, f: string) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_mdx_mode'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#mdx')).toBeVisible();
}
const phone = (page: Page) => (page.viewportSize()?.width || 1280) <= 700;

test('on a phone the source is the plain textarea at 16px, and typing saves', async ({ page }) => {
  test.skip(!phone(page), 'phone layout');
  const f = note();
  await open(page, f);
  await page.locator('#mdx-mode [data-m="edit"]').click();
  const ta = page.locator('#file-edit-ta');
  await expect(ta).toBeVisible();
  const look = await ta.evaluate((el: HTMLTextAreaElement) => {
    const cs = getComputedStyle(el), hl = document.getElementById('mdx-hl');
    return { size: parseFloat(cs.fontSize), color: cs.color, transparent: cs.color === 'rgba(0, 0, 0, 0)',
      hlShown: !!hl && getComputedStyle(hl).display !== 'none', hlLines: hl ? hl.childElementCount : 0 };
  });
  expect(look.size, 'at least 16px: smaller zooms iOS on focus').toBeGreaterThanOrEqual(16);
  expect(look.transparent, 'the text is the textarea\'s own').toBe(false);
  expect(look.hlShown, 'no second layer to drift').toBe(false);
  expect(look.hlLines, 'nothing built for it').toBe(0);
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(9, 9); });
  await page.keyboard.type('Today ');
  await expect.poll(() => readFileSync(f, 'utf8'), { timeout: 8000 }).toContain('# Notes\n\nToday ## Section 0');
  await expect(page.locator('#mdx-dirty')).toHaveText('Saved');
  await page.screenshot({ path: test.info().outputPath('md-phone-edit.png') });
});

test('a formatting button keeps the focus, so the phone keyboard stays up', async ({ page }) => {
  const f = note(5);
  await open(page, f);
  await page.locator('#mdx-mode [data-m="edit"]').click();
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(2, 7); });
  const kept = await page.evaluate(() => {
    const b = document.querySelector('#mdx-tools [data-t="bold"]') as HTMLElement;
    const ev = new PointerEvent('pointerdown', { bubbles: true, cancelable: true });
    b.dispatchEvent(ev);
    return ev.defaultPrevented;
  });
  expect(kept, 'pointerdown on a button is held, so the textarea keeps focus').toBe(true);
  await page.locator('#mdx-tools [data-t="bold"]').click();
  await expect(ta).toHaveValue(/^# \*\*Notes\*\*/);
  expect(await page.evaluate(() => document.activeElement?.id)).toBe('file-edit-ta');
});

test('a keystroke rewrites only the line it changed', async ({ page }) => {
  test.skip(phone(page), 'the highlighted source is the wide layout');
  const f = note();
  await open(page, f);
  await page.locator('#mdx-mode [data-m="edit"]').click();
  const r = await page.evaluate(() => {
    const ta = document.getElementById('file-edit-ta') as HTMLTextAreaElement, hl = document.getElementById('mdx-hl')!;
    const n = hl.childElementCount, first = hl.children[0], last = hl.children[n - 1], target = hl.children[2];
    ta.focus(); ta.setSelectionRange(ta.value.indexOf('## Section 0'), ta.value.indexOf('## Section 0'));
    ta.setRangeText('x', ta.selectionStart, ta.selectionStart, 'end');
    ta.dispatchEvent(new InputEvent('input', { bubbles: true }));
    // A new line too: nothing after it is rewritten or renumbered by hand.
    ta.setRangeText('\n', ta.selectionStart, ta.selectionStart, 'end');
    ta.dispatchEvent(new InputEvent('input', { bubbles: true }));
    return { lines: hl.childElementCount === ta.value.split('\n').length, grew: hl.childElementCount === n + 1,
      firstKept: hl.children[0] === first, lastKept: hl.children[hl.childElementCount - 1] === last,
      targetReplaced: !hl.contains(target), text: hl.textContent === ta.value + '\n' };
  });
  expect(r).toEqual({ lines: true, grew: true, firstKept: true, lastKept: true, targetReplaced: true, text: true });
});
