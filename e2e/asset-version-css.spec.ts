import { test, expect } from './fixtures';

// 2026-11-26: the page linked /app.css with no version while /app.js carried
// ?v=<build>. A cached sheet from the build before stayed in place under the
// new code: Manual-only dragging worked, but the old rules still showed the
// grip and grab cursor under every sort. The link now names its build, and a
// sheet from another build is reported.

test('the stylesheet link names the same build as app.js, and the sheet is that build', async ({ page }) => {
  await page.goto('/');
  const r = await page.evaluate(() => ({
    app: (window as any).eval('APP_VER') as string,
    js: new URL((document.querySelector('script[src*="/app.js"]') as HTMLScriptElement).src).searchParams.get('v'),
    css: new URL((document.querySelector('link[rel="stylesheet"][href*="/app.css"]') as HTMLLinkElement).href).searchParams.get('v'),
    sheet: getComputedStyle(document.documentElement).getPropertyValue('--amux-asset-ver').trim().replace(/^["']|["']$/g, ''),
  }));
  expect(r.css).toBe(r.js);
  expect(r.css).toBe(r.app);
  expect(r.sheet).toBe(r.app);
});

test('a stylesheet from another build is reported', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    try { beacons.push(JSON.parse(route.request().postData() || '{}')); } catch {}
    await route.fulfill({ json: { ok: true } });
  });
  await page.route(/\/app\.css(\?.*)?$/, async route => {
    const res = await route.fetch();
    const body = (await res.text()).replace(/--amux-asset-ver: "[^"]*"/, '--amux-asset-ver: "older-build"');
    await route.fulfill({ response: res, body });
  });
  await page.goto('/');
  await expect.poll(() => beacons.find(b => b.kind === 'asset-version-mismatch' && b.verdict === 'app_css_and_app_js_builds_differ')?.css_ver)
    .toBe('older-build');
});
