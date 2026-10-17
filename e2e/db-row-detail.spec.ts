// Database view: clicking or tapping a data row opens the row detail dialog
// (the shared chip-detail modal), and the dialog fits a phone screen.
import { test, expect, type Page } from './fixtures';

const LONG = 'segment-'.repeat(220) + 'END';
const PAYLOAD = JSON.stringify({ kind: 'fixture', tags: ['a', 'b'], nested: { deep: true } });

async function sql(page: Page, statement: string) {
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const r = await page.request.post('/api/sql', {
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    data: { sql: statement, write: true },
  });
  expect(r.ok(), statement + ' -> ' + (await r.text())).toBe(true);
}

async function openTable(page: Page) {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  await sql(page, 'CREATE TABLE IF NOT EXISTS wb_dbmodal (id INTEGER PRIMARY KEY, name TEXT, payload TEXT, notes TEXT, missing TEXT)');
  await sql(page, 'DELETE FROM wb_dbmodal');
  await sql(page, `INSERT INTO wb_dbmodal (id, name, payload, notes, missing) VALUES (1, 'alpha row', '${PAYLOAD}', '${LONG}', NULL), (2, 'beta row', '[1,2,3]', 'short', 'present')`);
  await page.evaluate(() => (window as any).switchView('sql'));
  const tbl = page.locator('#db-tables .db-tbl', { hasText: 'wb_dbmodal' });
  await expect(tbl).toBeVisible();
  await tbl.click();
  await expect(page.locator('#db-grid tbody tr.db-row')).toHaveCount(2);
}

const dialog = (page: Page) => page.locator('#db-row-detail');
const firstRow = (page: Page) => page.locator('#db-grid tbody tr.db-row').first();
const activeRow = (page: Page) => page.evaluate(() => (document.activeElement as HTMLElement | null)?.dataset?.row ?? null);

test('a row opens its full details and every close path returns focus to the row', async ({ page }) => {
  await openTable(page);
  await firstRow(page).locator('td').nth(1).click();
  const d = dialog(page);
  await expect(d).toBeVisible();
  await expect(d.locator('h3')).toHaveText('wb_dbmodal · id = 1');

  const item = (name: string) => d.locator('.db-detail-item', { has: page.locator('.db-detail-name', { hasText: new RegExp('^' + name + '$') }) });
  await expect(item('name').locator('dd')).toHaveText('alpha row');
  // Full value, no truncation.
  expect(await item('notes').locator('dd').innerText()).toBe(LONG);
  // JSON is pretty printed.
  await expect(item('payload').locator('pre.db-detail-json')).toHaveText(JSON.stringify(JSON.parse(PAYLOAD), null, 2));
  // NULL is shown as NULL, distinctly, and has nothing to copy.
  await expect(item('missing').locator('.sql-null')).toHaveText('NULL');
  await expect(item('missing').locator('.db-detail-copy')).toHaveCount(0);
  await expect(item('name').locator('.db-detail-copy')).toBeVisible();

  // Close button.
  await d.locator('.modal-close').click();
  await expect(d).toHaveCount(0);
  expect(await activeRow(page)).toBe('0');

  // Keyboard: Enter on the focused row opens, Esc closes.
  await firstRow(page).focus();
  await page.keyboard.press('Enter');
  await expect(d).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(d).toHaveCount(0);
  expect(await activeRow(page)).toBe('0');

  // Footer Close button.
  await page.locator('#db-grid tbody tr.db-row').nth(1).click();
  await expect(d.locator('h3')).toHaveText('wb_dbmodal · id = 2');
  await d.locator('.modal-footer [data-close]').click();
  await expect(d).toHaveCount(0);
  expect(await activeRow(page)).toBe('1');

  // Backdrop.
  await firstRow(page).click();
  await expect(d).toBeVisible();
  await d.click({ position: { x: 4, y: 3 } });
  await expect(d).toHaveCount(0);
  expect(await activeRow(page)).toBe('0');
});

test('sorting, text selection and the Structure mode still work', async ({ page }) => {
  await openTable(page);
  // Sort header click sorts and opens nothing.
  await page.locator('#db-grid th.sortable', { hasText: 'name' }).click();
  await expect(page.locator('#db-grid th.sortable', { hasText: 'name' })).toContainText('▲');
  await page.locator('#db-grid th.sortable', { hasText: 'name' }).click();
  await expect(page.locator('#db-grid th.sortable', { hasText: 'name' })).toContainText('▼');
  await expect(firstRow(page).locator('td').nth(1)).toHaveText('beta row');
  await expect(dialog(page)).toHaveCount(0);

  // Selecting cell text keeps the selection and opens nothing.
  const cell = firstRow(page).locator('td').nth(1);
  await cell.evaluate(td => {
    const range = document.createRange(); range.selectNodeContents(td);
    const s = window.getSelection()!; s.removeAllRanges(); s.addRange(range);
  });
  await cell.dispatchEvent('click');
  await expect(dialog(page)).toHaveCount(0);
  expect(await page.evaluate(() => String(window.getSelection()))).toContain('beta row');
  await page.evaluate(() => window.getSelection()!.removeAllRanges());

  // Structure mode renders its column table and opens nothing on click.
  await page.locator('#db-mode-structure').click();
  await expect(page.locator('#db-structure tbody tr')).toHaveCount(5);
  await page.locator('#db-structure tbody tr').first().click();
  await expect(dialog(page)).toHaveCount(0);
  await page.locator('#db-mode-data').click();
  await expect(page.locator('#db-grid')).toBeVisible();
});

test('row detail fits a 375px phone screen with 44px touch targets', async ({ page }, info) => {
  await page.setViewportSize({ width: 375, height: 667 });
  await openTable(page);
  if (info.project.name === 'desktop') await page.screenshot({ path: info.outputPath('grid-375.png') });
  await firstRow(page).click();
  const d = dialog(page);
  await expect(d).toBeVisible();
  const box = d.locator('.modal');
  const r = (await box.boundingBox())!;
  expect(r.x).toBeGreaterThanOrEqual(0);
  expect(r.x + r.width).toBeLessThanOrEqual(375 + 0.5);
  expect(r.y).toBeGreaterThanOrEqual(0);
  expect(r.y + r.height).toBeLessThanOrEqual(667 + 0.5);
  // No horizontal overflow anywhere: page, dialog, body, the long value.
  const overflow = await page.evaluate(() => {
    const m = document.querySelector('#db-row-detail .modal') as HTMLElement;
    const b = document.querySelector('#db-row-detail .modal-body') as HTMLElement;
    return {
      page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
      modal: m.scrollWidth - m.clientWidth,
      body: b.scrollWidth - b.clientWidth,
      scrolls: b.scrollHeight > b.clientHeight && getComputedStyle(b).overflowY,
    };
  });
  expect(overflow.page).toBeLessThanOrEqual(0);
  expect(overflow.modal).toBeLessThanOrEqual(0);
  expect(overflow.body).toBeLessThanOrEqual(0);
  // The long notes value makes the body scroll, inside the dialog.
  expect(overflow.scrolls).toBe('auto');
  for (const sel of ['.modal-close', '.modal-footer [data-close]', '.db-detail-copy']) {
    const b = (await d.locator(sel).first().boundingBox())!;
    expect(b.width, sel).toBeGreaterThanOrEqual(44);
    expect(b.height, sel).toBeGreaterThanOrEqual(44);
  }
  await expect.poll(() => page.evaluate(() => (window as any)._modalLayoutCheck().clipped)).toEqual([]);
  await page.screenshot({ path: info.outputPath('row-detail-375.png') });
  await d.locator('.modal-close').tap().catch(() => d.locator('.modal-close').click());
  await expect(d).toHaveCount(0);
});

test('row detail uses a wide dialog on a desktop screen', async ({ page }, info) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await openTable(page);
  await firstRow(page).click();
  await expect(dialog(page)).toBeVisible();
  const r = (await dialog(page).locator('.modal').boundingBox())!;
  expect(r.width).toBeGreaterThan(600);
  await page.screenshot({ path: info.outputPath('row-detail-desktop.png') });
});
