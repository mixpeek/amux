// End-to-end test of amux Cloud signup, as a visitor, on real infrastructure in Stripe test mode
// (cloud/start). Prints one JSON report; screenshots go to OUT.
//
//   API=https://amux-cloud-test.<sub>.workers.dev TK=<test key> OUT=/tmp/e2e \
//     node cloud/start/e2e.mjs
//
// Phases: signup page (desktop and mobile) -> bad input -> Stripe test checkout (4242) ->
// welcome page until Ready -> the server's door: wrong password, then the right one -> dashboard.
// Cancellation and cleanup are checked by e2e_after.py, which needs the Stripe and Hetzner keys.
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';

const API = process.env.API, TK = process.env.TK, OUT = process.env.OUT || '/tmp/amux-start-e2e';
const RUN = Date.now().toString(36);
const SLUG = process.env.SLUG || `e2e-${RUN}`;
const EMAIL = process.env.EMAIL || `hello+e2e-${RUN}@amux.io`;
const PW = `pw-${RUN}-correct-horse`;
mkdirSync(OUT, { recursive: true });
const report = { slug: SLUG, email: EMAIL, steps: [], console_errors: [], ok: false };
const t0 = Date.now();
const step = async (name, fn) => {
  const s = Date.now();
  try { const detail = await fn(); report.steps.push({ name, ok: true, ms: Date.now() - s, detail }); }
  catch (e) { report.steps.push({ name, ok: false, ms: Date.now() - s, error: String(e.message || e).slice(0, 400) }); throw e; }
};
const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 } });
const page = await ctx.newPage();
page.on('console', m => { if (m.type() === 'error') report.console_errors.push(m.text().slice(0, 200)); });
const shot = n => page.screenshot({ path: `${OUT}/${n}.png`, fullPage: true });
const startUrl = `https://amux.io/start/?plan=team&api=${encodeURIComponent(API)}&tk=${encodeURIComponent(TK)}&x=${RUN}`;
let paidAt;
try {
  await step('A0 homepage leads to /start', async () => {
    await page.goto(`https://amux.io/?x=${RUN}`);
    const hero = await page.locator('#hero-start').getAttribute('href');
    await page.locator('#start a[href="/start/?plan=team"]').scrollIntoViewIfNeeded();
    await shot('00-home-plans');
    await Promise.all([page.waitForURL(/\/start\/\?plan=team/), page.locator('#start a[href="/start/?plan=team"]').click()]);
    if (hero !== '/start/') throw new Error('hero CTA points at ' + hero);
    // The visitor's own URL from here on, plus this run's test function (live Stripe otherwise).
    return { hero, landed: new URL(page.url()).pathname + new URL(page.url()).search };
  });
  await step('A1 signup page (desktop)', async () => {
    await page.goto(startUrl);
    await page.getByText('Just trying amux?').waitFor();
    if (!(await page.locator('input[name=plan][value=team]').isChecked())) throw new Error('?plan=team not preselected');
    await shot('01-start-desktop');
    return 'free note, plans, form; team preselected';
  });
  await step('A1 signup page (390px)', async () => {
    const m = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await m.goto(startUrl); await m.getByText('Continue to payment').waitFor();
    const overflow = await m.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
    await m.screenshot({ path: `${OUT}/02-start-mobile.png`, fullPage: true }); await m.close();
    if (overflow) throw new Error('horizontal overflow at 390px');
    return 'no horizontal overflow';
  });
  await step('A2 bad input is refused with a message', async () => {
    await page.click('#go');
    const e1 = await page.locator('#err').innerText();
    const answer = async () => { await page.waitForFunction(() => !/^Checking/.test(document.querySelector('#slug-hint').innerText), null, { timeout: 15_000 });
                                 return page.locator('#slug-hint').innerText(); };
    await page.fill('#slug', 'cloud'); await page.waitForTimeout(500);
    const reserved = await answer();
    await page.fill('#slug', SLUG); await page.waitForTimeout(500);
    const avail = await answer();
    await page.fill('#email', EMAIL); await page.fill('#pw', 'short'); await page.click('#go');
    const e2 = await page.locator('#err').innerText();
    if (!e1 || !/reserved/i.test(reserved) || !/available/.test(avail) || !/10 characters/.test(e2)) throw new Error(JSON.stringify({ e1, reserved, avail, e2 }));
    return { e1, reserved, avail, e2 };
  });
  await step('A3 continue opens amux-branded Stripe checkout for Team', async () => {
    await page.fill('#pw', PW);
    await Promise.all([page.waitForURL(/pay\.|checkout\.stripe\.com/, { timeout: 60_000 }), page.click('#go')]);
    await page.getByText('amux Cloud Team').first().waitFor({ timeout: 30_000 });
    await shot('03-checkout');
    const body = await page.locator('body').innerText();
    if (/mixpeek/i.test(body.split('\n').slice(0, 5).join(' '))) throw new Error('checkout header shows Mixpeek');
    return new URL(page.url()).host;
  });
  await step('A4 pay with test card 4242', async () => {
    const card = page.frameLocator('iframe[name^="__privateStripeFrame"]').first();
    await page.fill('#cardNumber', '4242424242424242').catch(() => card.locator('[name=cardnumber]').fill('4242424242424242'));
    await page.fill('#cardExpiry', '12 / 34'); await page.fill('#cardCvc', '123');
    await page.fill('#billingName', 'E2E Customer');
    const manual = page.getByText('Enter address manually');
    if (await manual.isVisible().catch(() => false)) await manual.click();
    if (await page.locator('#billingAddressLine1').isVisible().catch(() => false)) {
      await page.selectOption('#billingCountry', 'US').catch(() => {});
      await page.fill('#billingAddressLine1', '1 Main St'); await page.fill('#billingPostalCode', '10001');
      await page.fill('#billingLocality', 'New York');
      await page.selectOption('#billingAdministrativeArea', 'NY').catch(() => {});
    }
    // Stripe ticks "Save my information with Link" for US addresses and then wants a phone number.
    const link = page.locator('#enableStripePass');
    if (await link.isChecked().catch(() => false)) await link.uncheck({ force: true });
    await Promise.all([page.waitForURL(/amux\.io\/start\/welcome/, { timeout: 120_000 }), page.click('button[type=submit]')]);
    paidAt = Date.now();
    await shot('04-welcome-start');
    return page.url().replace(/session_id=[^&]+/, 'session_id=<cs>');
  });
  await step('A4 welcome page reaches Ready', async () => {
    const seen = new Set();
    const deadline = Date.now() + 9 * 60_000;
    while (Date.now() < deadline) {
      seen.add(await page.locator('#steps li.now').innerText().catch(() => 'done'));
      if (await page.locator('#ready').isVisible()) break;
      await page.waitForTimeout(3000);
    }
    if (!(await page.locator('#ready').isVisible())) throw new Error('not ready after 9 minutes; saw ' + [...seen].join(' | '));
    await shot('05-welcome-ready');
    report.paid_to_ready_s = Math.round((Date.now() - paidAt) / 1000);
    return { states_seen: [...seen], paid_to_ready_s: report.paid_to_ready_s, link: await page.locator('#open').innerText() };
  });
  await step('A6 wrong password is refused', async () => {
    await page.click('#open'); await page.waitForURL(/_door\/login/, { timeout: 60_000 });
    await page.fill('input[name=username]', EMAIL); await page.fill('input[name=password]', 'wrong-password-123');
    await page.click('button[type=submit], input[type=submit]');
    await page.waitForLoadState('load');
    if (!/_door\/login/.test(page.url())) throw new Error('wrong password got in: ' + page.url());
    await shot('06-door-wrong');
    return (await page.locator('.err').innerText().catch(() => '')) || 'stayed on login';
  });
  await step('A5 right password opens the amux dashboard', async () => {
    await page.fill('input[name=username]', EMAIL); await page.fill('input[name=password]', PW);
    await page.click('button[type=submit], input[type=submit]');
    await page.locator('#brand-header').waitFor({ timeout: 90_000 });
    const api = await page.evaluate(async () => (await fetch('/api/sessions', { credentials: 'same-origin' })).status);
    await page.waitForTimeout(2500); await shot('07-dashboard');
    if (api !== 200) throw new Error('/api/sessions ' + api);
    return { url: new URL(page.url()).host, api_sessions: api };
  });
  report.ok = true;
} catch (e) {
  await shot('99-failure').catch(() => {});
} finally {
  report.total_s = Math.round((Date.now() - t0) / 1000);
  await browser.close();
  console.log(JSON.stringify(report, null, 1));
}
