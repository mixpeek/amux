import { test, expect } from './fixtures';
import { mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import { cleanup } from './teardown';

// Ethan, 2026-10-26: "fix the xlsx file viewer/editor in amux viewer. its off
// i click somewhere the cell clicked is wrong its not aligned". The editor
// draws on a canvas and maps the pointer for an unzoomed page; under amux's
// CSS zoom (Settings > Zoom, 110% on the owner's Mac app) the drawn grid and
// the hit test disagreed. Measured before the fix in Chromium at 125%: a click
// on row 20 selected row 13. A click lands on the cell drawn under it at every
// zoom level now.

declare const _xlsxEd: any;

test('a click selects the cell drawn under the pointer at 80%, 100% and 125% zoom', async ({ page }) => {
  test.setTimeout(150_000);   // a cold load of the editor's libraries
  const dir = await mkdtemp(join(tmpdir(), 'amux-xlsx-click-'));
  try {
    execFileSync('python3', [join(__dirname, 'file-view-fixtures.py'), dir, 'END-OF-FILE']);
    await page.goto('/');
    await page.setViewportSize({ width: 1280, height: 800 });
    for (const zoom of [100, 125, 80]) {
      await page.evaluate(z => { (0, eval)('_zoomLevel = ' + z); (window as any)._applyZoom(); }, zoom);
      await page.evaluate(p => (window as any).openFilePreview(p), join(dir, 'workbook.xlsx'));
      await page.waitForFunction(() => _xlsxEd && _xlsxEd.api && _xlsxEd.api.getActiveWorkbook(), null, { timeout: 90_000 });
      await page.waitForTimeout(1500);
      for (const [row, col] of [[1, 0], [3, 1], [2, 2]]) {
        const pt = await page.evaluate(([row, col]) => {
          const ws = _xlsxEd.api.getActiveWorkbook().getActiveSheet();
          const canvas = document.querySelector('#xlsx-univer canvas[id^="univer-sheet-main-canvas"]') as HTMLCanvasElement;
          const r = canvas.getBoundingClientRect();
          // Where the cell is DRAWN on screen: the canvas box over its drawing size.
          const sx = r.width / (canvas.width / devicePixelRatio), sy = r.height / (canvas.height / devicePixelRatio);
          let x = 46, y = 20;   // the editor's row-header width and column-header height
          for (let c = 0; c < col; c++) x += ws.getColumnWidth(c);
          for (let q = 0; q < row; q++) y += ws.getRowHeight(q);
          return { x: r.left + sx * (x + ws.getColumnWidth(col) / 2), y: r.top + sy * (y + ws.getRowHeight(row) / 2) };
        }, [row, col]);
        await page.mouse.click(pt.x, pt.y);
        await expect.poll(() => page.evaluate(() => {
          const r = _xlsxEd.api.getActiveWorkbook().getActiveSheet().getSelection().getActiveRange();
          return [r.getRow(), r.getColumn()];
        }), `zoom ${zoom}%: the cell under the pointer`).toEqual([row, col]);
      }
      await page.evaluate(() => (window as any).closeFilePreview());
    }
  } finally {
    await cleanup('remove the fixture folder', () => rm(dir, { recursive: true, force: true }));
  }
});

test('the Mac app is recognised as WebKit, so it never takes the CSS-zoom path', async ({ page }) => {
  await page.goto('/');
  const webkit = (ua: string) => page.evaluate(ua => {
    Object.defineProperty(navigator, 'userAgent', { value: ua, configurable: true });
    return (window as any)._isAppleWebKit();
  }, ua);
  // Mac app builds before 2026-10-26 sent only their own name.
  expect(await webkit('Amux-Desktop/2.0')).toBe(true);
  expect(await webkit('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Amux-Desktop/2.0')).toBe(true);
  expect(await webkit('Mozilla/5.0 (Macintosh) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0 Safari/537.36')).toBe(false);
});

// Excel parity (Ethan, 2026-10-26: "iterate until it has full parity with
// excel"). Opening, clicking and scrolling a workbook changes nothing in it, so
// it never asks to save. Escape belongs to the sheet: it cancels a half-typed
// cell and leaves the workbook open, as in Excel.
test('opening and clicking is not an edit, and Escape cancels a cell without closing the workbook', async ({ page }) => {
  test.setTimeout(150_000);
  const dir = await mkdtemp(join(tmpdir(), 'amux-xlsx-esc-'));
  try {
    execFileSync('python3', [join(__dirname, 'file-view-fixtures.py'), dir, 'END-OF-FILE']);
    await page.goto('/');
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.evaluate(p => (window as any).openFilePreview(p), join(dir, 'workbook.xlsx'));
    await page.waitForFunction(() => _xlsxEd && _xlsxEd.api && _xlsxEd.api.getActiveWorkbook(), null, { timeout: 90_000 });
    await page.waitForTimeout(1500);
    const canvas = page.locator('#xlsx-univer canvas[id^="univer-sheet-main-canvas"]');
    await canvas.click({ position: { x: 120, y: 60 } });
    // Mobile WebKit has no wheel events; scroll through the sheet there.
    if (test.info().project.name === 'ios-safari') await page.evaluate(() => _xlsxEd.api.getActiveWorkbook().getActiveSheet().scrollToCell(30, 0));
    else await page.mouse.wheel(0, 200);
    await page.waitForTimeout(500);
    expect(await page.evaluate(() => _xlsxEd.dirty), 'a click and a scroll are not edits').toBe(false);
    const before = await page.evaluate(() => {
      const r = _xlsxEd.api.getActiveWorkbook().getActiveSheet().getSelection().getActiveRange();
      return { a1: r.getA1Notation(), v: r.getValue() };
    });
    await page.keyboard.type('zzz');
    await page.waitForTimeout(300);
    await page.keyboard.press('Escape');
    await page.waitForTimeout(600);
    const after = await page.evaluate(a1 => ({
      open: !!_xlsxEd && document.getElementById('file-overlay')!.classList.contains('active'),
      v: _xlsxEd && _xlsxEd.api.getActiveWorkbook().getActiveSheet().getRange(a1).getValue(),
      dirty: _xlsxEd && _xlsxEd.dirty,
    }), before.a1);
    expect(after).toEqual({ open: true, v: before.v, dirty: false });
    await page.evaluate(() => (window as any).closeFilePreview());
  } finally {
    await cleanup('remove the fixture folder', () => rm(dir, { recursive: true, force: true }));
  }
});
