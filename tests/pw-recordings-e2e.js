const { chromium } = require('playwright');
const { homedir } = require('os');

(async () => {
  const ctx = await chromium.launchPersistentContext(
    `${homedir()}/.amux/playwright-auth/profile`,
    { headless: true, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
  );
  const page = await ctx.newPage();
  await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
  await page.waitForTimeout(1500);

  // 1. Test /api/recordings endpoint
  const recData = await page.evaluate(async () => {
    const r = await fetch('/api/recordings');
    return r.json();
  });
  console.log('1. /api/recordings:', JSON.stringify({ count: recData.recordings?.length, dir: recData.dir, first: recData.recordings?.[0]?.name }));

  // 2. Navigate to Browser tab
  await page.locator('#tab-browser').click();
  await page.waitForTimeout(500);
  await page.screenshot({ path: '/tmp/rec-e2e-01-browser-tab.png' });
  console.log('2. Browser tab opened');

  // 3. Check Recordings button exists
  const recBtn = await page.evaluate(() => {
    const btns = Array.from(document.querySelectorAll('#browser-view .btn'));
    const rb = btns.find(b => b.textContent.includes('Recordings'));
    return rb ? { found: true, text: rb.textContent.trim() } : { found: false };
  });
  console.log('3. Recordings button:', JSON.stringify(recBtn));

  // 4. Click Recordings button
  await page.locator('#browser-view .btn').filter({ hasText: 'Recordings' }).click();
  await page.waitForTimeout(1000);
  await page.screenshot({ path: '/tmp/rec-e2e-02-explore-open.png' });

  const exploreState = await page.evaluate(() => {
    const overlay = document.getElementById('explore-overlay');
    const rows = document.querySelectorAll('#explore-body .explore-row');
    const crumb = document.getElementById('explore-breadcrumb')?.textContent?.trim();
    return {
      active: overlay?.classList.contains('active'),
      rowCount: rows.length,
      breadcrumb: crumb,
      firstRow: rows[0]?.textContent?.trim()?.substring(0, 60),
    };
  });
  console.log('4. Explore overlay:', JSON.stringify(exploreState));

  // 5. Click the first .mp4 file in the recordings dir
  const mp4Row = page.locator('#explore-body .explore-row').filter({ hasText: '.mp4' }).first();
  const mp4Name = await mp4Row.textContent().catch(() => null);
  if (mp4Name) {
    console.log('5. Clicking MP4:', mp4Name.trim().substring(0, 50));
    await mp4Row.click();
    await page.waitForTimeout(1500);
    await page.screenshot({ path: '/tmp/rec-e2e-03-video-viewer.png' });

    const viewerState = await page.evaluate(() => {
      const overlay = document.getElementById('file-overlay');
      const body = document.getElementById('file-body');
      const video = body?.querySelector('video');
      const track = video?.querySelector('track');
      const meta = body?.querySelector('.file-video-meta');
      const dlBtn = document.getElementById('file-download-btn');
      return {
        overlayActive: overlay?.classList.contains('active'),
        bodyClass: body?.className,
        hasVideo: !!video,
        videoSrc: video?.src?.substring(0, 80),
        hasTrack: !!track,
        trackSrc: track?.src?.substring(0, 80),
        hasMeta: !!meta,
        metaText: meta?.textContent?.trim()?.substring(0, 100),
        dlBtnVisible: dlBtn?.style?.display !== 'none',
        dlBtnHref: dlBtn?.href?.substring(0, 60),
        fileTitle: document.getElementById('file-title')?.textContent,
      };
    });
    console.log('6. Video viewer state:', JSON.stringify(viewerState, null, 2));

    // 7. Test /api/file/vtt if track exists
    if (recData.recordings?.[0]?.srt) {
      const vttResp = await page.evaluate(async (srtPath) => {
        const r = await fetch('/api/file/vtt?path=' + encodeURIComponent(srtPath));
        const text = await r.text();
        return { status: r.status, startsWithWebvtt: text.startsWith('WEBVTT'), preview: text.substring(0, 80) };
      }, recData.recordings[0].srt);
      console.log('7. VTT conversion:', JSON.stringify(vttResp));
    }

    // 8. Close file viewer — verify video paused
    await page.locator('#file-overlay .btn').filter({ hasText: '✕' }).first().click();
    await page.waitForTimeout(300);
    const afterClose = await page.evaluate(() => ({
      overlayActive: document.getElementById('file-overlay')?.classList.contains('active'),
      dlBtnHidden: document.getElementById('file-download-btn')?.style?.display === 'none',
    }));
    console.log('8. After close:', JSON.stringify(afterClose));
    await page.screenshot({ path: '/tmp/rec-e2e-04-after-close.png' });
  } else {
    console.log('5. No MP4 rows found in explore');
  }

  // Summary
  console.log('\n━━━ Results ━━━');
  console.log(`/api/recordings works:    ${recData.recordings ? '✓ ' + recData.recordings.length + ' recordings' : '✗'}`);
  console.log(`dir returned:             ${recData.dir ? '✓ ' + recData.dir : '✗'}`);
  console.log(`Recordings button:        ${recBtn.found ? '✓' : '✗'}`);
  console.log(`Explore opened:           ${exploreState.active ? '✓' : '✗'}`);
  console.log(`Explore breadcrumb:       ${exploreState.breadcrumb || '(none)'}`);
  console.log(`MP4 rows in dir:          ${exploreState.rowCount}`);
  console.log('\nScreenshots: /tmp/rec-e2e-*.png');

  await ctx.close();
})();
