/**
 * pw-recordings-viewer.js
 * E2E test: open the recordings viewer, play the newest recording,
 * verify captions track, download button, and close behaviour.
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
  await page.waitForTimeout(1500);

  // ── 1. Fetch recordings list ──────────────────────────────────────────────
  const recData = await page.evaluate(async () => {
    const r = await fetch('/api/recordings');
    return r.json();
  });
  const newest = recData.recordings?.[0];
  console.log('1. Newest recording:', newest?.name, newest?.srt ? '(has SRT)' : '(no SRT)');
  if (!newest) { console.log('ERROR: no recordings found'); await ctx.close(); return; }

  // ── 2. Open Browser tab → click Recordings button ────────────────────────
  await page.locator('#tab-browser').click();
  await page.waitForTimeout(400);
  await page.locator('#browser-view .btn').filter({ hasText: 'Recordings' }).click();
  await page.waitForTimeout(800);
  await page.screenshot({ path: '/tmp/rv-01-recordings-dir.png' });

  const dirState = await page.evaluate(() => {
    const crumb = document.getElementById('explore-breadcrumb')?.textContent?.trim();
    const rows = [...document.querySelectorAll('#explore-body .explore-row')].map(r => r.textContent.trim().substring(0, 50));
    return { crumb, rows };
  });
  console.log('2. Recordings dir breadcrumb:', dirState.crumb);
  console.log('   Rows:', dirState.rows.slice(0, 5));

  // ── 3. Click the newest MP4 ───────────────────────────────────────────────
  const mp4Locator = page.locator('#explore-body .explore-row').filter({ hasText: newest.name });
  await mp4Locator.click();
  await page.waitForTimeout(2000);  // give video element time to render + metadata to load
  await page.screenshot({ path: '/tmp/rv-02-video-viewer-open.png' });

  const viewerState = await page.evaluate(() => {
    const body = document.getElementById('file-body');
    const video = body?.querySelector('video');
    const track = video?.querySelector('track');
    const meta = body?.querySelector('.file-video-meta');
    const dlBtn = document.getElementById('file-download-btn');
    const title = document.getElementById('file-title');
    return {
      overlayActive: document.getElementById('file-overlay')?.classList.contains('active'),
      bodyClass: body?.className,
      hasVideo: !!video,
      videoSrc: video?.src ? 'set' : 'empty',
      videoReadyState: video?.readyState,    // 0=NO_DATA 1=METADATA 2=CURRENT_DATA 3=FUTURE_DATA 4=ENOUGH_DATA
      videoDuration: video?.duration,
      hasTrack: !!track,
      trackKind: track?.kind,
      trackSrc: track?.src ? 'set' : 'empty',
      hasMeta: !!meta,
      metaText: meta?.textContent?.trim()?.replace(/\s+/g, ' '),
      captionsLabel: track?.label,
      dlBtnVisible: dlBtn?.style?.display !== 'none',
      dlBtnHref: dlBtn?.href ? 'set' : 'empty',
      fileTitle: title?.textContent,
    };
  });
  console.log('3. Video viewer:', JSON.stringify(viewerState, null, 2));

  // ── 4. Verify VTT conversion ──────────────────────────────────────────────
  if (newest.srt) {
    const vtt = await page.evaluate(async (srtPath) => {
      const r = await fetch('/api/file/vtt?path=' + encodeURIComponent(srtPath));
      const text = await r.text();
      const lines = text.split('\n').slice(0, 8);
      return { status: r.status, isVtt: text.startsWith('WEBVTT'), lineCount: text.split('\n').length, preview: lines };
    }, newest.srt);
    console.log('4. VTT:', JSON.stringify(vtt));
  }

  // ── 5. Seek the video forward to confirm it's playable ───────────────────
  await page.evaluate(() => {
    const v = document.querySelector('#file-body video');
    if (v && v.duration) v.currentTime = v.duration * 0.3;
  });
  await page.waitForTimeout(800);
  await page.screenshot({ path: '/tmp/rv-03-video-seeked.png' });
  const seekState = await page.evaluate(() => {
    const v = document.querySelector('#file-body video');
    return { currentTime: v?.currentTime, duration: v?.duration };
  });
  console.log('5. Seek state:', JSON.stringify(seekState));

  // ── 6. Close the viewer ───────────────────────────────────────────────────
  await page.locator('#file-overlay .btn').filter({ hasText: '✕' }).first().click();
  await page.waitForTimeout(300);
  const afterClose = await page.evaluate(() => ({
    overlayActive: document.getElementById('file-overlay')?.classList.contains('active'),
    dlBtnHidden: document.getElementById('file-download-btn')?.style?.display === 'none',
    videoGone: !document.querySelector('#file-body video') || document.querySelector('#file-body video')?.src === '',
  }));
  console.log('6. After close:', JSON.stringify(afterClose));
  await page.screenshot({ path: '/tmp/rv-04-after-close.png' });

  // ── Summary ───────────────────────────────────────────────────────────────
  console.log('\n━━━ Results ━━━');
  console.log(`Recording found:        ✓ ${newest.name}`);
  console.log(`Has SRT:                ${newest.srt ? '✓' : '✗ (no companion .srt)'}`);
  console.log(`Dir browse opened:      ${dirState.crumb?.includes('recordings') ? '✓' : '✗'}`);
  console.log(`File overlay active:    ${viewerState.overlayActive ? '✓' : '✗'}`);
  console.log(`file-video CSS class:   ${viewerState.bodyClass?.includes('file-video') ? '✓' : '✗'}`);
  console.log(`Video element present:  ${viewerState.hasVideo ? '✓' : '✗'}`);
  console.log(`Video src set:          ${viewerState.videoSrc === 'set' ? '✓' : '✗'}`);
  console.log(`Video has metadata:     ${viewerState.videoReadyState >= 1 ? '✓ readyState=' + viewerState.videoReadyState : '✗ readyState=' + viewerState.videoReadyState}`);
  console.log(`Captions track:         ${viewerState.hasTrack ? '✓ kind=' + viewerState.trackKind : (newest.srt ? '✗ missing!' : 'n/a (no .srt)')}`);
  console.log(`Metadata bar:           ${viewerState.hasMeta ? '✓ ' + viewerState.metaText?.substring(0, 60) : '✗'}`);
  console.log(`Download btn visible:   ${viewerState.dlBtnVisible ? '✓' : '✗'}`);
  console.log(`Duration:               ${viewerState.videoDuration ? '✓ ' + viewerState.videoDuration?.toFixed(1) + 's' : '✗ no duration'}`);
  console.log(`Seek worked:            ${seekState.currentTime > 0 ? '✓ ' + seekState.currentTime?.toFixed(1) + 's' : '✗'}`);
  console.log(`Close hides overlay:    ${!afterClose.overlayActive ? '✓' : '✗'}`);
  console.log(`Close hides dl btn:     ${afterClose.dlBtnHidden ? '✓' : '✗'}`);
  console.log('\nScreenshots: /tmp/rv-0*.png');

  await ctx.close();
})();
