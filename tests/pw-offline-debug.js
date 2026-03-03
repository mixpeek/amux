const { chromium } = require('playwright');
const { homedir } = require('os');

(async () => {
  const ctx = await chromium.launchPersistentContext(
    `${homedir()}/.amux/playwright-auth/profile`,
    { headless: true, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
  );
  const page = await ctx.newPage();

  // Capture console output
  const logs = [];
  page.on('console', msg => logs.push(msg.type() + ': ' + msg.text()));
  page.on('pageerror', err => logs.push('ERROR: ' + err.message));

  await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
  await page.waitForTimeout(2000);
  await page.click('#tab-files');
  await page.waitForTimeout(800);

  // Online: load dir + file
  page.evaluate(() => loadFiles('/Users/ethan/Dev/amux'));
  await page.waitForTimeout(2000);
  page.evaluate(() => openFilePreview('/Users/ethan/Dev/amux/README.md'));
  await page.waitForTimeout(2500);
  page.evaluate(() => closeFilePreview());
  await page.waitForTimeout(500);

  // Verify IDB write happened by reading it back synchronously-ish
  const idbVersion = await page.evaluate(() => new Promise(resolve => {
    const req = indexedDB.open('amux');
    req.onsuccess = () => { resolve({ version: req.result.version, stores: [...req.result.objectStoreNames] }); req.result.close(); };
    req.onerror = () => resolve({ error: 'open failed' });
  }));
  console.log('IDB info:', JSON.stringify(idbVersion));

  // Go offline
  await ctx.setOffline(true);
  await page.waitForTimeout(500);
  console.log('Went offline');

  // Test: does fetch fail for localhost?
  const fetchResult = await page.evaluate(async () => {
    try {
      const r = await fetch('https://localhost:8822/api/ls?path=/tmp');
      return { status: r.status, ok: r.ok };
    } catch(e) {
      return { error: e.message, type: e.constructor.name };
    }
  });
  console.log('Fetch to localhost when offline:', JSON.stringify(fetchResult));

  // Test loadFiles manually
  page.evaluate(() => loadFiles('/Users/ethan/Dev/amux'));
  await page.waitForTimeout(3000);
  const bodyHtml = await page.evaluate(() => document.getElementById('files-body').innerHTML.substring(0, 200));
  console.log('Files body offline:', bodyHtml);

  console.log('\nConsole logs:', logs.slice(-10).join('\n'));
  await ctx.setOffline(false);
  await ctx.close();
})();
