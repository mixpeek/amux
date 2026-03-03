const { chromium } = require('playwright');
const { homedir } = require('os');

(async () => {
  const ctx = await chromium.launchPersistentContext(
    `${homedir()}/.amux/playwright-auth/profile`,
    { headless: false, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
  );
  const page = await ctx.newPage();
  await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
  await page.waitForTimeout(1500);

  // Go to Browser tab → click Recordings
  await page.locator('#tab-browser').click();
  await page.waitForTimeout(400);
  await page.locator('#browser-view .btn').filter({ hasText: 'Recordings' }).click();
  await page.waitForTimeout(800);

  // Click the newest .mp4
  const newestData = await page.evaluate(async () => {
    const r = await fetch('/api/recordings');
    const d = await r.json();
    return d.recordings?.[0];
  });
  console.log('Opening:', newestData?.name);

  await page.locator('#explore-body .explore-row').filter({ hasText: newestData?.name }).click();
  await page.waitForTimeout(2000);

  // Let user watch — keep open for 60 seconds
  console.log('Video viewer open. Press Ctrl+C to close early.');
  await page.waitForTimeout(60000);
  await ctx.close();
})();
