// Cmd-K finds files you open (Ethan, 2026-10-30: "cmd k should include files
// as well but needs to be intelligent about picking which files should be
// displayed (ones ive visited frequently/recent etc.)", with his To-Do.md
// missing when he typed "to-do"). The server records each file the dashboard
// opens (api/file_visits.rs) and /api/palette offers them ranked by visits and
// recency. Real server, no stubs: open a file, then find it.
import { test, expect } from './fixtures';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

test('a file you opened is found by name in Cmd-K, and opens from there', async ({ page }) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cmdk-files-'));
  const file = path.join(dir, 'To-Do.md');
  fs.writeFileSync(file, '# To do\n\n- [ ] palette files\n');
  try {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openCmdk === 'function' && typeof (window as any).openFilePreview === 'function');
    // Open it the way the dashboard does, twice: each view is a visit.
    for (let i = 0; i < 2; i++) {
      const status = await page.evaluate(p => fetch('/api/file?path=' + encodeURIComponent(p), { headers: (window as any)._authHeaders() }).then(r => r.status), file);
      expect(status).toBe(200);
    }
    await expect.poll(async () => page.evaluate(() => fetch('/api/palette?q=to-do', { headers: (window as any)._authHeaders() })
      .then(r => r.json()).then(d => (d.hits || []).filter((h: any) => h.kind === 'file').map((h: any) => h.title))), { timeout: 15000 })
      .toContain('To-Do.md');

    await page.evaluate(() => (window as any).openCmdk());
    await page.locator('#cmdk-input').fill('to-do');
    const row = page.locator('#cmdk-list .cmdk-row', { hasText: 'To-Do.md' });
    await expect(row).toBeVisible();
    await expect(row).toContainText('File');
    await row.click();
    await expect(page.locator('body')).toContainText('palette files', { timeout: 10000 });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
