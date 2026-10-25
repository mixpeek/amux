// Ethan, 2026-10-22: "allow me to export logs from the logs page". Export
// downloads what the open tab shows: Raw Logs as a .log file (up to the
// server's 5,000-line tail, with the line filter applied), Activity as JSON.
import { test, expect } from './fixtures';
import fs from 'fs';

test('the Logs page exports raw logs and activity', async ({ page }) => {
  test.setTimeout(60_000);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('logs'));
  await page.locator('#lst-raw').click();
  await expect(page.locator('#logs-raw-body')).not.toBeEmpty({ timeout: 20_000 });

  const first = page.waitForEvent('download');
  await page.locator('#logs-export').click();
  const all = await first;
  expect(all.suggestedFilename()).toMatch(/^amux-logs-.*\.log$/);
  const text = fs.readFileSync(await all.path(), 'utf8');
  const lines = text.trim().split('\n');
  expect(lines.length).toBeGreaterThan(0);
  expect(text).not.toContain('\u001b[');

  // The line filter applies to the file.
  const word = 'INFO';
  await page.locator('#raw-filter').fill(word);
  const dl = page.waitForEvent('download');
  await page.locator('#logs-export').click();
  const filtered = fs.readFileSync(await (await dl).path(), 'utf8').trim().split('\n').filter(Boolean);
  expect(filtered.length).toBeGreaterThan(0);
  expect(filtered.every(l => l.toLowerCase().includes(word.toLowerCase()))).toBeTruthy();

  await page.locator('#lst-activity').click();
  const act = page.waitForEvent('download');
  await page.locator('#logs-export').click();
  const a = await act;
  expect(a.suggestedFilename()).toMatch(/^amux-activity-.*\.json$/);
  expect(Array.isArray(JSON.parse(fs.readFileSync(await a.path(), 'utf8')))).toBeTruthy();
});
