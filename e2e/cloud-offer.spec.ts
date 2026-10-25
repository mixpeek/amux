// The amux Cloud offer for self-hosted owners (MSG-91014). Every e2e server is
// self-hosted and the browser holds the owner token, so the real /api/identity
// is the self-hosted owner case; the Cloud cases mock that one response.
// The offer's state is the server pref `cloud_offer`, reset before each test.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

test.describe.configure({ mode: 'serial' });
// Opts out of the harness's "dismissed" seed for this spec (fixtures.ts).
const OPT = { annotation: { type: 'cloud-offer' } };

async function token(page: Page) {
  await page.waitForFunction(() => typeof (window as any)._AMUX_AUTH_TOKEN === 'string');
  const t = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  return t ? { Authorization: `Bearer ${t}` } : {};
}

async function resetPref(page: Page) {
  const r = await page.request.post('/api/prefs', { headers: await token(page), data: { key: 'cloud_offer', value: '' } });
  expect(r.ok()).toBeTruthy();
}

async function boot(page: Page) {
  // The first load carries the harness's "dismissed" seed, so it neither shows
  // nor records anything while the server pref is reset. The reload then clears
  // the seed once; later reloads keep whatever the app itself wrote.
  await page.addInitScript(() => {
    if (sessionStorage.getItem('cloud_offer_clear') === '1') {
      sessionStorage.setItem('cloud_offer_clear', 'done');
      localStorage.removeItem('amux_cloud_offer');
    }
  });
  await page.goto('/');
  await resetPref(page);
  await page.evaluate(() => sessionStorage.setItem('cloud_offer_clear', '1'));
  await page.reload();
}

async function verdict(page: Page) {
  await page.waitForFunction(() => (window as any)._cloudOfferVerdict?.measured === true);
  return page.evaluate(() => (window as any)._cloudOfferVerdict);
}

// The offer's state is ONE server pref, so any page on the same e2e server can
// spend the trigger first. When the offer never shows, say what the pref held,
// so that race reads as a race instead of a missing offer.
async function expectOfferShown(page: Page) {
  try {
    await expect(page.locator('#cloud-offer')).toBeVisible({ timeout: 10000 });
  } catch (e) {
    const r = await page.request.get('/api/prefs?key=cloud_offer', { headers: await token(page) });
    throw new Error(`#cloud-offer never showed; the server's cloud_offer pref is ${await r.text()}. `
      + 'A trigger this page did not record means another context on this server spent it '
      + '(every context must start with the fixtures.ts dismissed seed).\n' + String(e));
  }
}

async function openSettingsAccount(page: Page) {
  await page.evaluate(() => { (window as any).toggleSettings(); (window as any)._settingsTab('account'); });
}

test('phone: a self-hosted owner is offered Cloud once, and a dismissal holds across reloads', OPT, async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 700 });
  await boot(page);
  const v = await verdict(page);
  expect(v.owner).toBe(true);
  expect(v.phone).toBe(true);

  const offer = page.locator('#cloud-offer');
  await expectOfferShown(page);
  await expect(offer).toHaveAttribute('data-trigger', 'phone');
  await expect(offer).toContainText('even when this computer sleeps');
  await expect(page.locator('#cloud-offer-go')).toHaveAttribute('href', 'https://amux.io/start/?source=app-phone');
  // Fits a 375px screen with 44px touch targets.
  await expect(offer).toBeInViewport({ ratio: 1 });
  for (const id of ['#cloud-offer-go', '#cloud-offer-dismiss']) {
    const b = await page.locator(id).boundingBox();
    expect(b!.height).toBeGreaterThanOrEqual(44);
  }
  await page.screenshot({ path: test.info().outputPath('cloud-offer-phone.png') });
  const text = await offer.innerText();
  expect(text).not.toContain('—');

  await page.locator('#cloud-offer-dismiss').click();
  await expect(offer).toHaveCount(0);
  await expect.poll(async () => {
    const r = await page.request.get('/api/prefs?key=cloud_offer', { headers: await token(page) });
    return JSON.parse((await r.json()).value || '{}').dismissed > 0;
  }).toBe(true);

  await page.reload();
  expect((await verdict(page)).owner).toBe(true);
  await page.waitForTimeout(2500);   // past the 1.5s show delay
  await expect(offer).toHaveCount(0);

  // Another device (no local copy) reads the server's dismissal.
  await page.evaluate(() => localStorage.removeItem('amux_cloud_offer'));
  await page.reload();
  await verdict(page);
  await page.waitForTimeout(2500);
  await expect(offer).toHaveCount(0);
});

test('a trigger shows once: a reload without dismissing does not repeat it', OPT, async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 700 });
  await boot(page);
  await expectOfferShown(page);
  await page.reload();
  await verdict(page);
  await page.waitForTimeout(2500);
  await expect(page.locator('#cloud-offer')).toHaveCount(0);
});

test('long job: a worker active 2+ hours after its last message gets the longjob pitch', OPT, async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await boot(page);
  expect((await verdict(page)).owner).toBe(true);
  const now = Date.now();
  // Short of 2 hours: nothing.
  await page.evaluate((now) => (0, eval)(`sessions = [{name:'e2e-short', status:'active', last_human_ts:${now - 3600e3}}]; _cloudOfferCheckLongJob();`), now);
  await page.waitForTimeout(800);
  await expect(page.locator('#cloud-offer')).toHaveCount(0);
  await page.evaluate((now) => (0, eval)(`sessions = [{name:'e2e-long', status:'active', last_human_ts:${now - 3 * 3600e3}}]; _cloudOfferCheckLongJob();`), now);
  const offer = page.locator('#cloud-offer');
  await expect(offer).toBeVisible();
  await expect(offer).toHaveAttribute('data-trigger', 'longjob');
  await expect(offer).toContainText('Keep long jobs running when your laptop closes');
  await expect(offer).toContainText('e2e-long');
  await expect(page.locator('#cloud-offer-go')).toHaveAttribute('href', 'https://amux.io/start/?source=app-longjob');
});

test('Settings: a self-hosted owner has a quiet amux Cloud entry', OPT, async ({ page }) => {
  await boot(page);
  expect((await verdict(page)).owner).toBe(true);
  await openSettingsAccount(page);
  const sec = page.locator('#settings-cloud-offer');
  await expect(sec).toBeVisible();
  await expect(sec).toContainText('amux export');
  await expect(sec).toContainText('amux import');
  await expect(page.locator('#settings-cloud-offer-link')).toHaveAttribute('href', 'https://amux.io/start/?source=app-settings');
  await sec.scrollIntoViewIfNeeded();
  await page.screenshot({ path: test.info().outputPath('cloud-offer-settings.png') });
});

for (const [label, identity] of [
  ['gateway user (is_cloud)', { email: 'owner@example.com', is_cloud: true, is_local_member: false }],
  ['dedicated Cloud server owner (account_url)', { email: '', is_cloud: false, is_local_member: false, account_url: 'https://amux.io/account/' }],
  ['invited member (is_local_member)', { email: 'member@example.com', is_cloud: false, is_local_member: true, access_scope: { level: 'global', name: '' } }],
] as const) {
  test(`not offered to a ${label}`, OPT, async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 700 });
    await boot(page);
    await page.route('**/api/identity', route => route.fulfill({
      status: 200, contentType: 'application/json',
      body: JSON.stringify({ has_api_key: true, has_any_model: true, key_valid: null, key_error: '', ...identity }),
    }));
    await page.reload();
    const v = await verdict(page);
    expect(v.owner).toBe(false);
    expect(v.phone).toBe(true);   // the phone trigger condition held, so only ownership kept it hidden
    await page.waitForTimeout(2500);
    await expect(page.locator('#cloud-offer')).toHaveCount(0);
    await openSettingsAccount(page);
    await expect(page.locator('#settings-cloud-offer')).toBeHidden();
  });
}

// A context a spec opens itself gets the same seed as the fixture's, so its
// phone-width pages cannot spend the server-wide trigger (run 39968514404:
// golden_realtime_convergence did, in the other worker, mid-test above).
test('a context a spec opens itself starts with the offer dismissed', async ({ browser, baseURL }) => {
  const ctx = await browser.newContext({ baseURL, ignoreHTTPSErrors: true, viewport: { width: 375, height: 700 } });
  try {
    const page = await ctx.newPage();
    await page.goto('/');
    expect(await page.evaluate(() => localStorage.getItem('amux_cloud_offer'))).toBe('{"dismissed":1}');
    expect(await page.evaluate(() => localStorage.getItem('amux_walkthrough_done'))).toBe('1');
  } finally {
    await cleanup('close the spec-made context', () => ctx.close());
  }
});
