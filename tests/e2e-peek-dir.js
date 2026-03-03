/**
 * e2e-peek-dir.js
 * Tests: clicking the directory path in the Peek overlay opens the explore overlay.
 */
const { chromium } = require('playwright');
const { homedir } = require('os');

(async () => {
  const ctx = await chromium.launchPersistentContext(
    `${homedir()}/.amux/playwright-auth/profile`,
    { headless: true, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
  );
  const page = await ctx.newPage();
  await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
  await page.waitForTimeout(2000);

  await page.screenshot({ path: '/tmp/e2e-01-home.png' });
  console.log('1. Dashboard loaded');

  // Find a session that has a directory set
  const sessionWithDir = await page.evaluate(() => {
    const cards = document.querySelectorAll('.card[data-session]');
    for (const c of cards) {
      const dirEl = c.querySelector('.card-dir-path');
      if (dirEl && dirEl.textContent.trim()) {
        return { name: c.dataset.session, dir: dirEl.textContent.trim() };
      }
    }
    return null;
  });
  console.log('2. Session with dir:', JSON.stringify(sessionWithDir));
  if (!sessionWithDir) { console.log('ERROR: no session with dir'); await ctx.close(); return; }

  // Open the ⋯ menu on that card and click "Peek terminal"
  const menuBtn = page.locator(`.card[data-session="${sessionWithDir.name}"] .card-menu-btn`).first();
  await menuBtn.click({ force: true });
  await page.waitForTimeout(500);
  await page.screenshot({ path: '/tmp/e2e-02-menu-open.png' });
  console.log('3. Card menu opened');

  // Click "Peek terminal" menu item
  await page.locator('.card-menu-item').filter({ hasText: 'Peek terminal' }).first().click();
  await page.waitForTimeout(1500);
  await page.screenshot({ path: '/tmp/e2e-03-peek-open.png' });
  console.log('4. Peek overlay opened');

  // Verify peek overlay is active
  const peekActive = await page.evaluate(() =>
    document.getElementById('peek-overlay')?.classList.contains('active') ?? false
  );
  console.log('5. Peek overlay active:', peekActive);

  // Check the dir bar state
  const dirBarInfo = await page.evaluate(() => {
    const el = document.getElementById('peek-dir-text');
    if (!el) return { found: false };
    const cs = window.getComputedStyle(el);
    return {
      found: true,
      text: el.textContent.trim(),
      cursor: cs.cursor,
      textDecoration: cs.textDecoration,
      hasOnclick: !!el.getAttribute('onclick'),
    };
  });
  console.log('6. Dir bar state:', JSON.stringify(dirBarInfo, null, 2));

  if (!dirBarInfo.text) {
    console.log('ERROR: dir text is empty — peek may not have loaded dir yet');
    await ctx.close();
    return;
  }

  // Click the directory path text
  await page.locator('#peek-dir-text').click({ force: true });
  await page.waitForTimeout(1500);
  await page.screenshot({ path: '/tmp/e2e-04-after-dir-click.png' });
  console.log('7. Directory path clicked');

  // Verify the explore overlay opened
  const overlayState = await page.evaluate(() => {
    const overlay = document.getElementById('explore-overlay');
    const body = document.getElementById('explore-body');
    const rows = body ? body.querySelectorAll('.explore-row') : [];
    const pinnedRows = body ? body.querySelectorAll('.explore-row-pinned') : [];
    return {
      overlayActive: overlay?.classList.contains('active') ?? false,
      rowCount: rows.length,
      pinnedCount: pinnedRows.length,
      breadcrumb: document.getElementById('explore-breadcrumb')?.textContent?.trim() ?? null,
      firstRowText: rows[0]?.textContent?.trim()?.substring(0, 60) ?? null,
    };
  });
  console.log('8. Explore overlay state:', JSON.stringify(overlayState, null, 2));

  await page.screenshot({ path: '/tmp/e2e-05-explore-open.png' });

  // Navigate into a subdirectory to confirm directory browsing works
  const subdir = page.locator('#explore-body .explore-row').filter({ hasNotText: '..' }).first();
  const subdirText = await subdir.textContent().catch(() => null);
  if (subdirText) {
    console.log('9. Clicking into:', subdirText.trim().substring(0, 40));
    await subdir.click();
    await page.waitForTimeout(1000);
    const newBreadcrumb = await page.evaluate(() =>
      document.getElementById('explore-breadcrumb')?.textContent?.trim() ?? null
    );
    console.log('10. New breadcrumb:', newBreadcrumb);
    await page.screenshot({ path: '/tmp/e2e-06-subdir.png' });
  }

  // Summary
  console.log('\n━━━ Results ━━━');
  console.log(`Peek opened:          ${peekActive ? '✓' : '✗'}`);
  console.log(`Dir text shown:       ${dirBarInfo.text ? '✓ ' + dirBarInfo.text : '✗ empty'}`);
  console.log(`Cursor pointer:       ${dirBarInfo.cursor === 'pointer' ? '✓' : '✗ ' + dirBarInfo.cursor}`);
  console.log(`Has onclick:          ${dirBarInfo.hasOnclick ? '✓' : '✗'}`);
  console.log(`Explore opened:       ${overlayState.overlayActive ? '✓' : '✗'}`);
  console.log(`Explore breadcrumb:   ${overlayState.breadcrumb ?? '(none)'}`);
  console.log(`Rows in listing:      ${overlayState.rowCount}`);
  console.log(`Pinned rows:          ${overlayState.pinnedCount}`);
  console.log('\nScreenshots: /tmp/e2e-0*.png');

  await ctx.close();
})();
