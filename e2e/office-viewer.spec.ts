import {test, expect, Page} from './fixtures';
import {mkdtemp, copyFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
declare const _xlsxEd: any;
declare const _xlsxSave: any;

// Ethan, 2026-10-27: "for anything like docx or xlsx make sure we maintain
// product specific capabilities like comments ... a native sota docx viewer so
// we can see inline comments and everything, do the same with pptx".
// Fixtures: e2e/fixtures/office/make-office-fixtures.py.
const FIX = join(__dirname, 'fixtures', 'office');

test.beforeEach(async ({page}) => {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: []}));
});

async function openFixture(page: Page, name: string) {
  const dir = await mkdtemp(join(tmpdir(), 'amux-office-'));
  await copyFile(join(FIX, name), join(dir, name));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate(p => (window as any).openFilePreview(p), join(dir, name));
  return join(dir, name);
}

const narrow = (page: Page) => (page.viewportSize()?.width || 1280) <= 900;

async function showPanel(page: Page) {
  if (narrow(page)) {
    // On a phone the panel is a sheet behind one floating button: no new bar.
    await expect(page.locator('#file-body .office-panel')).not.toBeInViewport();
    await page.locator('#file-body .office-panel-fab').click();
  }
  await expect(page.locator('#file-body .office-panel')).toBeInViewport();
}

test('a Word document shows its pages, every comment with who, when and the text it is on, and its tracked changes', async ({page}) => {
  test.setTimeout(90_000);
  await openFixture(page, 'review.docx');
  const doc = page.locator('#file-body .office-doc');
  await expect(doc).toContainText('Quarterly review', {timeout: 30000});
  await expect(doc.locator('ins')).toContainText('deliberately');
  await showPanel(page);
  const items = page.locator('#file-body .office-item');
  await expect(items).toHaveCount(3);
  await expect(items.nth(0)).toContainText('Dana Reviewer');
  await expect(items.nth(0)).toContainText('Cite the source for 18 percent.');
  await expect(items.nth(0).locator('.office-item-quote')).toContainText('Revenue grew 18 percent');
  await expect(items.nth(1)).toContainText('Lee Editor');
  await expect(items.nth(1)).toContainText('Which three regions?');
  await expect(items.nth(2)).toContainText('Inserted');
  await expect(items.nth(2)).toContainText('deliberately');
  await expect(page.locator('#file-body .office-panel-head')).toContainText('2 comments · 1 change');
  // A comment jumps to its place in the document.
  await items.nth(1).click();
  await expect(doc.locator('.docx_commentreference.office-flash')).toHaveCount(1);
  await expect(doc.locator('.docx_commentreference.office-flash')).toContainText('Which three regions?');
  await expect.poll(() => doc.locator('.docx_commentreference.office-flash').evaluate(el => {
    const r = el.getBoundingClientRect(); return r.top >= 0 && r.bottom <= innerHeight;
  })).toBe(true);
});

test('a PowerPoint deck shows its slides with each slide\'s speaker notes and comments', async ({page}) => {
  test.setTimeout(90_000);
  await openFixture(page, 'deck.pptx');
  const doc = page.locator('#file-body .office-doc');
  await expect(doc).toContainText('Retrieval at scale', {timeout: 30000});
  await expect(doc).toContainText('October results');
  await showPanel(page);
  const panel = page.locator('#file-body .office-panel');
  await expect(panel.locator('.office-panel-group')).toHaveText(['Slide 1', 'Slide 2']);
  const items = panel.locator('.office-item');
  await expect(items).toHaveCount(3);
  await expect(items.nth(0)).toContainText('Speaker notes');
  await expect(items.nth(0)).toContainText('Open with the customer story.');
  await expect(items.nth(1)).toContainText('Pause here for questions.');
  await expect(items.nth(2)).toContainText('Sam Presenter');
  await expect(items.nth(2)).toContainText('Swap this chart for the October numbers.');
  await items.nth(2).click();
  await expect(doc.locator('.office-flash')).toContainText('October results');
});

test('an Excel cell comment shows as a native note and in the panel, and an edited note is saved into the file', async ({page}) => {
  test.setTimeout(120_000);
  const path = await openFixture(page, 'budget.xlsx');
  await expect(page.locator('#xlsx-univer canvas').first()).toBeVisible({timeout: 45000});
  await expect.poll(() => page.evaluate(() => _xlsxEd && _xlsxEd.api.getActiveWorkbook().getActiveSheet().getNotes().map((n: any) => [n.row, n.col, n.note])), {timeout: 20000})
    .toEqual([[1, 1, expect.stringContaining('Quote from the October vendor call.')]]);
  await showPanel(page);
  const item = page.locator('#file-body .office-item');
  await expect(item).toHaveCount(1);
  await expect(item).toContainText('Budget!B2');
  await expect(item).toContainText('Quote from the October vendor call.');
  // Change the note in the sheet and save: the file on disk carries the new text.
  await page.evaluate(async () => {
    _xlsxEd.api.getActiveWorkbook().getActiveSheet().getRange(1, 1).createOrUpdateNote({note: 'Revised after the November quote.', width: 220, height: 96});
    await _xlsxSave();
  });
  await expect(page.locator('#file-save-btn')).toHaveText(/Saved|Save/);
  const saved = await page.evaluate(async p => {
    const buf = await (await fetch('/api/file/raw?path=' + encodeURIComponent(p), {cache: 'no-store'})).arrayBuffer();
    const wb = new (window as any).ExcelJS.Workbook();
    await wb.xlsx.load(buf);
    const n = wb.getWorksheet('Budget').getCell('B2').note;
    return typeof n === 'string' ? n : (n && n.texts || []).map((t: any) => t.text).join('');
  }, path);
  expect(saved).toContain('Revised after the November quote.');
});

// Ethan, 2026-10-28: "speaker notes in pptx view is fucked". With fifteen long
// notes the panel's flex column shrank every item to its 44px floor and the
// text ran over the next slide's heading. Items keep their height and the
// list scrolls.
test('many long speaker notes each keep their own height and the list scrolls', async ({page}) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._officeCommentsMount === 'function');
  const r = await page.evaluate(() => {
    const host = document.createElement('div');
    host.style.cssText = 'display:flex;height:420px;width:700px;position:fixed;top:0;left:0;z-index:9999';
    document.body.appendChild(host);
    const text = 'Thanks for making time. Three things today: where round two landed against your criteria, how we would fit your stack, and the commercial shape. '.repeat(3);
    const items = Array.from({length: 15}, (_, i) => ({kind: 'note', group: 'Slide ' + (i + 1), text}));
    (window as any)._officeCommentsMount(host, items, null, {format: 'pptx'});
    const list = host.querySelector('.office-panel-list') as HTMLElement;
    const els = Array.from(host.querySelectorAll('.office-panel-list > *')) as HTMLElement[];
    const overflowing = els.filter(e => e.scrollHeight > e.clientHeight + 1).length;
    let overlaps = 0;
    for (let i = 1; i < els.length; i++) if (els[i].getBoundingClientRect().top < els[i - 1].getBoundingClientRect().bottom - 1) overlaps++;
    return {n: els.length, overflowing, overlaps, scrolls: list.scrollHeight > list.clientHeight};
  });
  expect(r.n).toBe(30);
  expect(r.overflowing).toBe(0);
  expect(r.overlaps).toBe(0);
  expect(r.scrolls).toBe(true);
});
