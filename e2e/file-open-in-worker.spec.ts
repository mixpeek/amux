// The file menu (owner, 2026-10-31): on a phone a sheet from the bottom with
// "Copy text", "Copy path" and "Copy link" spelled out, and "Open in worker":
// send to a new or existing worker, instructions with Summarize / Find issues /
// Make changes, the file attached as an @path, and the worker opening after.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const W = 'fow-w';

async function open(page: Page) {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-fow-'));
  const f = join(dir, 'plan.md');
  writeFileSync(f, '# Plan\n\n- one\n');
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: '/tmp' }] }));
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(w => typeof (window as any).openFilePreview === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await expect(page.locator('#mdx')).toBeVisible();
  return f;
}

test('the file menu names each copy, and on a phone it is a sheet at the bottom', async ({ page }) => {
  await open(page);
  await page.locator('#file-menu-btn').click();
  const menu = page.locator('#file-actions');
  await expect(menu).toBeVisible();
  for (const label of ['Open in worker…', 'Copy text', 'Copy path', 'Copy link'])
    await expect(menu.getByRole('button', { name: label, exact: true })).toBeVisible();
  const vw = page.viewportSize()!;
  if (vw.width <= 600) {
    const box = (await menu.boundingBox())!;
    expect(Math.abs(box.y + box.height - vw.height)).toBeLessThanOrEqual(2);
    expect(box.width).toBeGreaterThanOrEqual(vw.width - 2);
  }
  await page.screenshot({ path: test.info().outputPath('file-menu.png') });
});

test('Open in worker sends the instructions and the file to an existing worker, then opens it', async ({ page }) => {
  const f = await open(page);
  let sent = '';
  await page.route('**/api/sessions/' + W + '/send', async r => { sent = r.request().postDataJSON().text; await r.fulfill({ json: { ok: true } }); });
  await page.locator('#file-menu-btn').click();
  await page.locator('#file-tab-worker').click();
  await expect(page.locator('.fow-file')).toContainText('plan.md');
  await expect(page.locator('.fow-file')).toContainText('No unsaved edits');
  await page.locator('#fow-worker-field').waitFor();
  await page.locator('input[name="fow-to"][value="new"]').check();
  await expect(page.locator('#fow-worker-field')).toBeHidden();
  await page.locator('input[name="fow-to"][value="existing"]').check();
  await expect(page.locator('#fow-worker')).toHaveValue(W);
  await page.locator('.fow-chips [data-chip="issues"]').click();
  await expect(page.locator('#fow-text')).toHaveValue('Find issues in this file and list them.');
  await page.screenshot({ path: test.info().outputPath('open-in-worker.png') });
  await page.locator('#modal-btns button', { hasText: 'Open in worker' }).click();
  // doSend stamps the time in front, as for anything typed to a worker.
  await expect.poll(() => sent).toMatch(/Find issues in this file and list them\.\n@(.+)$/);
  expect(sent.endsWith('\n@' + f)).toBe(true);
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
});

test('Open in worker with no instructions sends nothing', async ({ page }) => {
  await open(page);
  let sends = 0;
  await page.route('**/api/sessions/' + W + '/send', async r => { sends++; await r.fulfill({ json: { ok: true } }); });
  allowUnusedRoute(page, '**/api/sessions/' + W + '/send');
  await page.locator('#file-menu-btn').click();
  await page.locator('#file-tab-worker').click();
  await page.locator('#modal-btns button', { hasText: 'Open in worker' }).click();
  await expect(page.locator('#toast')).toContainText('Say what the worker should do');
  expect(sends).toBe(0);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/);
});
