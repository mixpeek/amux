// The Browser tab's Import dialog imports sign-ins per site (owner,
// 2026-11-22, from the Athena teardown: Athena imports cookies per chosen
// site). The import itself is proven against a real profile by
// tests/browser-import-sites-e2e.py; this spec proves the dialog: the sites
// field is there, what is typed in it reaches the request as `sites`, the
// result says which sites and how many cookies were left out, and leaving it
// empty sends no sites. Discover and import are stubbed here so the test
// never reads a browser on the machine running it.
import { test, expect } from './fixtures';

const SOURCES = { sources: [{ id: 'firefox', name: 'Firefox', cookie_support: 'full', profiles: [{ name: 'e2e.default', display_name: 'e2e', default: true }] }] };

async function openImport(page: any, sent: any[]) {
  await page.route('**/api/browser/import/discover', (r: any) => r.fulfill({ json: SOURCES }));
  await page.route('**/api/browser/import', async (r: any) => {
    const body = JSON.parse(r.request().postData() || '{}');
    sent.push(body);
    const n = body.sites && body.sites.length ? 2 : 5;
    await r.fulfill({ json: { profile: body.destination, imported_cookies: n, skipped_cookies: 5 - n, warnings: [] } });
  });
  await page.goto('/#view=browser');
  await page.waitForFunction(() => typeof (window as any)._bwImportProfile === 'function');
  const btn = page.locator('button[onclick="_bwImportProfile()"]');
  await btn.scrollIntoViewIfNeeded();
  await btn.click();
  await expect(page.locator('#bwi-sites')).toBeVisible();
}

test('sites typed in the Import dialog go to the import, and the result names them', async ({ page }) => {
  const sent: any[] = [];
  await openImport(page, sent);
  await page.locator('#bwi-dest').fill('google-only');
  await page.locator('#bwi-sites').fill('google.com, https://github.com/login');
  await page.locator('button.btn.primary', { hasText: /^Import$/ }).click();
  await expect.poll(() => sent.length).toBe(1);
  expect(sent[0]).toMatchObject({ browser_id: 'firefox', profile_name: 'e2e.default', destination: 'google-only',
    sites: ['google.com', 'https://github.com/login'] });
  await expect(page.locator('#bw-status')).toContainText('Imported 2 cookies for google.com, https://github.com/login');
  await expect(page.locator('#bw-status')).toContainText('3 for other sites left out');
});

test('an empty sites field imports every cookie', async ({ page }) => {
  const sent: any[] = [];
  await openImport(page, sent);
  await page.locator('#bwi-dest').fill('everything');
  await page.locator('button.btn.primary', { hasText: /^Import$/ }).click();
  await expect.poll(() => sent.length).toBe(1);
  expect(sent[0].sites).toEqual([]);
  await expect(page.locator('#bw-status')).toContainText('Imported 5 cookies into profile');
});
