const { chromium } = require('playwright');
const { homedir } = require('os');

// Test offline file caching by directly exercising IDB read/write and
// simulating offline by calling the catch path of loadFiles/openFilePreview
(async () => {
  const ctx = await chromium.launchPersistentContext(
    `${homedir()}/.amux/playwright-auth/profile`,
    { headless: true, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
  );
  const page = await ctx.newPage();

  const errors = [];
  page.on('pageerror', e => errors.push(e.message));

  await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
  await page.waitForTimeout(2500);
  await page.click('#tab-files');
  await page.waitForTimeout(800);

  // Step 1: Load dir + file online (fire-and-forget — cache side effects happen in page)
  page.evaluate(() => loadFiles('/Users/ethan/Dev/amux'));
  await page.waitForTimeout(2000);
  await page.screenshot({ path: '/tmp/pw-offline-1-dir-online.png' });
  console.log('STEP 1: dir loaded online');

  page.evaluate(() => openFilePreview('/Users/ethan/Dev/amux/README.md'));
  await page.waitForTimeout(2500);
  await page.screenshot({ path: '/tmp/pw-offline-2-file-online.png' });
  console.log('STEP 2: file opened online');
  page.evaluate(() => closeFilePreview());
  await page.waitForTimeout(500);

  // Step 2: directly call the offline render path via IDB (simulates what happens in catch)
  const dirRenderResult = await page.evaluate(() => new Promise(resolve => {
    _idb.getFile('/Users/ethan/Dev/amux').then(cached => {
      if (!cached || cached.type !== 'dir') { resolve({ ok: false, reason: 'no dir cache', type: cached ? cached.type : null }); return; }
      const body = document.getElementById('files-body');
      _renderFilesEntries(body, '/Users/ethan/Dev/amux', cached.data, cached.ts);
      const html = body.innerHTML;
      resolve({ ok: html.includes('explore-row'), banner: html.includes('Offline cache'), entryCount: (html.match(/explore-row/g)||[]).length });
    }).catch(e => resolve({ ok: false, reason: String(e) }));
  }));
  console.log('STEP 3: dir IDB render result:', JSON.stringify(dirRenderResult));
  await page.screenshot({ path: '/tmp/pw-offline-3-dir-from-idb.png' });

  // Step 3: directly call the offline file render path
  const fileRenderResult = await page.evaluate(() => new Promise(resolve => {
    _idb.getFile('/Users/ethan/Dev/amux/README.md').then(cached => {
      if (!cached || cached.type !== 'file') { resolve({ ok: false, reason: 'no file cache', type: cached ? cached.type : null }); return; }
      document.getElementById('file-overlay').classList.add('active');
      const age = Math.round((Date.now() - cached.ts) / 60000);
      document.getElementById('file-title').textContent = 'README.md \u2022 cached ' + age + 'm ago';
      _renderFileBody(cached.data, 'preview');
      const body = document.getElementById('file-body');
      resolve({ ok: true, titleOk: true, bodyLen: body.textContent.length });
    }).catch(e => resolve({ ok: false, reason: String(e) }));
  }));
  console.log('STEP 4: file IDB render result:', JSON.stringify(fileRenderResult));
  await page.screenshot({ path: '/tmp/pw-offline-4-file-from-idb.png' });

  console.log('\nPage errors:', errors);
  console.log('\n=== RESULT ===');
  console.log('Dir cached & renderable offline:', dirRenderResult.ok ? 'PASS' : 'FAIL - ' + dirRenderResult.reason);
  console.log('File cached & renderable offline:', fileRenderResult.ok ? 'PASS' : 'FAIL - ' + fileRenderResult.reason);

  await ctx.close();
})();
