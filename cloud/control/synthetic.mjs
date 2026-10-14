// The synthetic customer (docs/design/cloud-self-service.md section 12): every 6 hours and on
// every deploy, a real browser walks the paid path and reports each step to the control plane,
// which the weekly report reads. It deletes its own account at the end, so it leaves nothing.
//
//   CONTROL_URL=https://cloud.amux.io SYNTH_EMAIL=synthetic+<run>@amux.io CONTROL_ADMIN_TOKEN=... \
//   [SYNTH_COUPON=SYNTH100] node cloud/control/synthetic.mjs
//
// Checkout: Stripe test mode takes card 4242; live mode takes a 100% coupon (SYNTH_COUPON).
// In CONTROL_FAKE mode the fake checkout page is used and the run stops at the door hand-off,
// because there is no real server behind it.
import { chromium } from 'playwright';

const CP = (process.env.CONTROL_URL || 'https://cloud.amux.io').replace(/\/$/, '');
const EMAIL = process.env.SYNTH_EMAIL || `synthetic+${Date.now()}@amux.io`;
const ADMIN = process.env.CONTROL_ADMIN_TOKEN;
const steps = [];
const t0 = Date.now();
let ok = false;

async function step(name, fn) {
  const s = Date.now();
  try {
    const detail = await fn();
    steps.push({ name, ok: true, ms: Date.now() - s, detail: detail ?? null });
  } catch (e) {
    steps.push({ name, ok: false, ms: Date.now() - s, error: String(e.message || e).slice(0, 300) });
    throw e;
  }
}

const browser = await chromium.launch();
const page = await (await browser.newContext({ viewport: { width: 390, height: 844 } })).newPage();
let fake = false, doorUrl = null;
try {
  await step('open pricing link', async () => {
    await page.goto(`${CP}/?plan=solo&source=synthetic`);
    fake = page.url().includes('/fake/checkout/');
    return page.url().replace(/\?.*/, '');
  });
  await step('pay', async () => {
    if (fake) {
      await page.fill('input[name=email]', EMAIL);
      await Promise.all([page.waitForURL(/\/progress/, { timeout: 60_000 }), page.click('#pay')]);
      return 'fake checkout';
    }
    await page.fill('input[name=email]', EMAIL);
    if (process.env.SYNTH_COUPON) {
      await page.getByText('Add promotion code').click();
      await page.fill('input[name=promotionCode]', process.env.SYNTH_COUPON);
      await page.keyboard.press('Enter');
    } else {
      const card = page.frameLocator('iframe[name^="__privateStripeFrame"]').first();
      await page.fill('#cardNumber', '4242424242424242').catch(() => card.locator('[name=cardnumber]').fill('4242424242424242'));
      await page.fill('#cardExpiry', '12 / 34');
      await page.fill('#cardCvc', '123');
      await page.fill('#billingName', 'Synthetic Customer');
    }
    await Promise.all([page.waitForURL(/\/progress/, { timeout: 120_000 }), page.click('button[type=submit]')]);
    return process.env.SYNTH_COUPON ? 'coupon' : 'test card';
  });
  await step('server ready and door hand-off', async () => {
    doorUrl = await new Promise((resolve, reject) => {
      page.on('request', r => { if (r.url().includes('/_door/enter')) resolve(r.url().replace(/t=.*/, 't=<token>')); });
      setTimeout(() => reject(new Error('no door hand-off within 15 minutes')), fake ? 20_000 : 15 * 60_000);
    });
    return doorUrl;
  });
  if (!fake) {
    await step('dashboard loads', async () => {
      await page.waitForLoadState('domcontentloaded');
      await page.waitForSelector('#apikey-setup-modal, .card, #sessions', { timeout: 60_000 });
      return new URL(page.url()).host;
    });
    await step('delete the synthetic account', async () => {
      const r = await page.request.post(`${CP}/api/account/delete`, { data: { confirm: 'delete' } });
      if (r.status() !== 202) throw new Error(`delete returned ${r.status()}`);
      return 'deletion started';
    });
  }
  ok = true;
} catch (e) {
  await page.screenshot({ path: `synthetic-failure-${Date.now()}.png` }).catch(() => {});
} finally {
  await browser.close();
}
const run = { ok, fake, ms: Date.now() - t0, steps };
console.log(JSON.stringify(run, null, 1));
if (ADMIN) {
  const r = await fetch(`${CP}/api/admin/synthetic`, { method: 'POST', headers: { Authorization: `Bearer ${ADMIN}`, 'Content-Type': 'application/json' },
    body: JSON.stringify(run) }).catch(e => ({ status: 0, e }));
  if (r.status !== 200) console.error(`could not record the run (HTTP ${r.status}); the report will show it as not measured`);
}
process.exit(ok ? 0 : 1);
