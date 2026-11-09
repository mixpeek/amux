// The file viewer's ⋯ menu carries what an OS file menu does (Ethan,
// 2026-11-08: "make it so i can print files have all the file options like i
// have for any os for files ... then test with pdf"): open in a new window,
// show in folder, print, download, duplicate, rename, move, get info and
// delete. Exercised on a real PDF in every project, the 375px phone included.
import { test, expect, Page } from './fixtures';
import { mkdtempSync, writeFileSync, existsSync, readFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// A one-page PDF that says "amux print test", with a correct xref table.
function onePagePdf(): Buffer {
  const objs = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
    null,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  const text = 'BT /F1 24 Tf 72 700 Td (amux print test) Tj ET';
  objs[3] = '<< /Length ' + text.length + ' >>\nstream\n' + text + '\nendstream';
  let out = '%PDF-1.4\n';
  const offs: number[] = [];
  objs.forEach((o, i) => { offs.push(out.length); out += (i + 1) + ' 0 obj\n' + o + '\nendobj\n'; });
  const xref = out.length;
  out += 'xref\n0 ' + (objs.length + 1) + '\n0000000000 65535 f \n' + offs.map(o => String(o).padStart(10, '0') + ' 00000 n \n').join('');
  out += 'trailer\n<< /Size ' + (objs.length + 1) + ' /Root 1 0 R >>\nstartxref\n' + xref + '\n%%EOF\n';
  return Buffer.from(out, 'latin1');
}

type Beacon = { kind?: string; action?: string; verdict?: string };
async function openPdf(page: Page) {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-fmenu-'));
  const f = join(dir, 'report.pdf');
  writeFileSync(f, onePagePdf());
  const beacons: Beacon[] = [];
  page.on('request', r => {
    if (!r.url().includes('/api/client-debug') || r.method() !== 'POST') return;
    try { const b = r.postDataJSON(); if (b && b.kind === 'file-action') beacons.push(b); } catch { /* not JSON */ }
  });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/);
  return { dir, f, beacons };
}
async function menu(page: Page) {
  await page.locator('#file-menu-btn').click();
  await expect(page.locator('#file-actions')).toBeVisible();
}

test('the ⋯ menu offers every file action, and Print prints the PDF itself', async ({ page }) => {
  const { f, beacons } = await openPdf(page);
  await menu(page);
  for (const label of ['Open in new window', 'Show in folder', 'Print', 'Download', 'Copy path', 'Copy link',
    'Rename', 'Duplicate', 'Move to', 'Get info', 'Delete']) {
    await expect(page.locator('#file-actions button:visible', { hasText: label }), label).toHaveCount(1);
  }
  // Every item fits the window (the phone sheet scrolls rather than clipping).
  const box = (await page.locator('#file-actions').boundingBox())!;
  expect(box.y + box.height).toBeLessThanOrEqual(page.viewportSize()!.height + 1);

  await page.locator('#file-tab-print').click();
  const frame = page.locator('#file-print-frame');
  await expect(frame).toHaveAttribute('data-kind', 'pdf');
  // The frame holds the file's own bytes as a PDF, not a picture of the viewer.
  const got = await frame.evaluate(async (el: HTMLIFrameElement) => {
    const b = await (await fetch(el.src)).blob();
    return { type: b.type, size: b.size, head: await b.slice(0, 5).text() };
  });
  expect(got).toEqual({ type: 'application/pdf', size: readFileSync(f).length, head: '%PDF-' });
  await expect.poll(() => beacons.filter(b => b.action === 'print').map(b => b.verdict)).toEqual(['print_opened']);
  await expect(frame).toHaveAttribute('data-printed', '1');
});

test('Get info, Duplicate, Rename, Move and Delete act on the PDF', async ({ page }) => {
  test.setTimeout(90_000);
  const { dir, f, beacons } = await openPdf(page);
  const size = readFileSync(f).length;

  await menu(page);
  await page.locator('#file-tab-info').click();
  await expect(page.locator('#modal-msg .file-info-table')).toContainText(size.toLocaleString('en-US') + ' bytes');
  await expect(page.locator('#modal-msg .file-info-table')).toContainText('PDF file');
  await page.locator('#modal-btns .btn.primary').click();

  await menu(page);
  await page.locator('#file-tab-duplicate').click();
  await expect.poll(() => existsSync(join(dir, 'report copy.pdf')), { timeout: 20_000 }).toBe(true);
  await expect.poll(() => existsSync(join(dir, 'report copy.pdf')) && readFileSync(join(dir, 'report copy.pdf')).length, { timeout: 20_000 }).toBe(size);

  await menu(page);
  await page.locator('#file-tab-rename').click();
  await page.locator('#modal-prompt-input').fill('renamed.pdf');
  await page.locator('#modal-prompt-input').press('Enter');
  await expect.poll(() => existsSync(join(dir, 'renamed.pdf'))).toBe(true);
  expect(existsSync(f)).toBe(false);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/);
  await expect(page.locator('#file-title')).toContainText('renamed');

  const sub = join(dir, 'archive');
  mkdirSync(sub);
  await menu(page);
  await page.locator('#file-tab-move').click();
  await page.locator('#modal-prompt-input').fill(sub);
  await page.locator('#modal-prompt-input').press('Enter');
  await expect.poll(() => existsSync(join(sub, 'renamed.pdf'))).toBe(true);
  expect(existsSync(join(dir, 'renamed.pdf'))).toBe(false);

  await menu(page);
  await page.locator('#file-tab-delete').click();
  await page.locator('#modal-btns .btn', { hasText: 'Delete' }).click();
  await expect.poll(() => existsSync(join(sub, 'renamed.pdf'))).toBe(false);
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);

  const seen = beacons.map(b => b.action + ':' + b.verdict);
  for (const v of ['info:shown', 'duplicate:queued', 'rename:renamed', 'move:moved', 'delete:deleted']) expect(seen).toContain(v);
});

test('Show in folder opens Files at the folder that holds the file', async ({ page }) => {
  const { dir } = await openPdf(page);
  await menu(page);
  await page.locator('#file-tab-reveal').click();
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  const leaf = dir.split('/').pop()!;
  await expect.poll(() => page.evaluate(() => String((0, eval)('_filesPath')))).toMatch(new RegExp('/' + leaf + '$'));
  await expect(page.locator('.fe-row[data-path$="/report.pdf"]').first()).toBeVisible();
});

test('Show in folder with unsaved edits still opens the folder under the file', async ({ page }) => {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-fmenu-md-'));
  const f = join(dir, 'notes.txt');
  writeFileSync(f, 'one\n');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await page.locator('#file-tab-edit').click();
  await page.locator('#file-edit-ta').fill('one\ntwo\n');
  await menu(page);
  await page.locator('#file-tab-reveal').click();
  // The edits are not thrown away: the save-or-discard prompt asks first.
  await expect(page.locator('#modal-backdrop')).toHaveClass(/open/);
  const leaf = dir.split('/').pop()!;
  await expect.poll(() => page.evaluate(() => String((0, eval)('_filesPath')))).toMatch(new RegExp('/' + leaf + '$'));
  expect(readFileSync(f, 'utf8')).toBe('one\n');
});
