import { chromium } from 'playwright';
// Run against a local server of site/:  (cd site && python3 -m http.server 8731) &
// then: node tests/site-funnel.e2e.mjs   (SITE=http://host:port to point elsewhere)
// PostHog is kept as its stub, so no event reaches production.
const B = process.env.SITE || 'http://localhost:8731';
const b = await chromium.launch();
let pass = 0, fail = 0;
const ok = (c, m) => { c ? pass++ : fail++; console.log((c ? 'ok   ' : 'FAIL ') + m); };
async function page(opts = {}) {
  const ctx = await b.newContext({ viewport: opts.mobile ? { width: 390, height: 844 } : { width: 1440, height: 900 } });
  await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: B });
  const p = await ctx.newPage();
  const errs = []; p.on('pageerror', e => errs.push(e.message));
  // Keep PostHog a stub so nothing reaches production and calls stay inspectable.
  await p.route(/posthog\.com|cloud\.amux\.io/, r => r.abort());
  p.errs = errs;
  return p;
}
const calls = p => p.evaluate(() => Array.from(window.posthog).filter(Array.isArray).map(c => [c[0], c[1], c[2]]));
const evs = async (p, name) => (await calls(p)).filter(c => c[0] === 'capture' && c[1] === name).map(c => c[2]);

// Homepage, arriving from ChatGPT
let p = await page();
await p.goto(B + '/', { referer: 'https://chatgpt.com/' });
await p.evaluate(() => document.addEventListener('click', e => e.preventDefault()));
let c = await calls(p);
const once = c.find(x => x[0] === 'register_once');
ok(once && once[1].first_source === 'ai' && once[1].first_landing === '/', 'first touch from chatgpt.com is source ai, landing /');
ok(c.some(x => x[0] === 'register_for_session' && x[1].session_source === 'ai'), 'session source registered');
await p.click('.site-header a[href="https://github.com/mixpeek/amux"]');
await p.click('.hero a[href="https://github.com/mixpeek/amux"]');
await p.click('.hero a[href^="/start/"]');
await p.click('#hero-ios');
await p.click('#hero-mac');
await p.click('a[href="/start/?plan=team"]');
await p.click('.hero .copy-btn').catch(() => {});
let cta = await evs(p, 'cta_click');
const has = (g, s) => cta.some(e => e.goal === g && e.spot === s);
ok(has('github', 'header'), 'header star click -> goal github, spot header');
ok(has('github', 'hero'), 'hero star click -> goal github, spot hero');
ok(has('cloud', 'hero'), 'hero cloud click -> goal cloud, spot hero');
ok(cta.find(e => e.goal === 'cloud' && e.spot === 'hero')?.href === '/start/?source=home', 'hero cloud link carries source=home');
ok(await p.evaluate(() => /\/home\/demo-desktop\.mp4$/.test(document.getElementById('demo-desk').currentSrc)), 'desktop hero loads the dashboard demo clip');
ok(await p.evaluate(() => new Promise(r => { const v = document.getElementById('demo-desk'); const t0 = v.currentTime; setTimeout(() => r(v.currentTime > t0), 1500); })), 'the demo clip is playing');
ok(await p.locator('.site-header a.nav-cta-primary').count() === 0, 'homepage header has no Get started: the hero holds the four calls to action');
ok(has('ios', 'hero'), 'hero iPhone app -> goal ios');
ok(has('mac', 'hero'), 'hero Mac app -> goal mac, not github');
ok(cta.some(e => e.goal === 'cloud' && e.spot === 'get' && e.plan === 'team'), 'plan link -> goal cloud, spot get, plan team');
const ic = await evs(p, 'install_copy');
ok(ic.length === 1 && ic[0].spot === 'hero', 'hero copy -> install_copy spot hero (' + JSON.stringify(ic) + ')');
await p.click('.site-header nav > a[href="/docs/"]');
ok((await evs(p, 'cta_click')).length === cta.length, 'non-goal link (docs) sends no cta_click');
ok(p.errs.length === 0, 'no JS errors on homepage ' + p.errs.join('|'));

// A content page from Google, mobile
p = await page({ mobile: true });
await p.goto(B + '/guides/harness-engineering/', { referer: 'https://www.google.com/' });
await p.evaluate(() => document.addEventListener('click', e => e.preventDefault()));
c = await calls(p);
ok(c.find(x => x[0] === 'register_once')?.[1].first_source === 'search', 'google referrer -> search');
await p.click('.site-header a[href*="github.com/mixpeek/amux"]');
cta = await evs(p, 'cta_click');
ok(cta.length === 1 && cta[0].goal === 'github' && cta[0].spot === 'header', 'mobile guide header star -> github/header');
await p.click('.site-header a[href="/#get"]');
cta = await evs(p, 'cta_click');
ok(cta.some(e => e.goal === 'pricing' && e.spot === 'header'), 'guide header Get started -> goal pricing, spot header');
ok(p.errs.length === 0, 'no JS errors on guide');

// Internal navigation does not overwrite first touch
p = await page();
await p.goto(B + '/pricing/', { referer: B + '/' });
c = await calls(p);
ok(!c.some(x => x[0] === 'register_once'), 'internal referrer registers no first touch');

// Concierge: "Schedule a meeting" is that page's goal
p = await page();
await p.goto(B + '/concierge/');
await p.evaluate(() => document.addEventListener('click', e => e.preventDefault()));
await p.click('a.js-schedule >> nth=0');
cta = await evs(p, 'cta_click');
ok(cta.length === 1 && cta[0].goal === 'concierge', 'Schedule a meeting -> goal concierge ' + JSON.stringify(cta));
ok(p.errs.length === 0, 'no JS errors on concierge ' + p.errs.join('|'));

// Signup page
p = await page();
let checkoutBody = null;
await p.route('**/api/checkout', r => { checkoutBody = JSON.parse(r.request().postData()); r.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"test"}' }); });
await p.goto(B + '/start/?plan=solo');
await p.focus('#slug');
ok((await evs(p, 'signup_form_start')).length === 1, 'focusing the form -> signup_form_start');
await p.focus('#email');
ok((await evs(p, 'signup_form_start')).length === 1, 'signup_form_start fires once');
await p.fill('#slug', 'funneltest'); await p.fill('#email', 'a@example.com'); await p.fill('#pw', 'abcdefghijk');
await p.click('#go');
await p.waitForFunction(() => !document.getElementById('go').disabled, null, { timeout: 20000 });
const sub = await evs(p, 'signup_submit');
ok(sub.length === 1 && sub[0].plan === 'solo' && sub[0].test === false, 'submit -> signup_submit plan solo ' + JSON.stringify(sub));
const er = await evs(p, 'signup_error');
ok(er.length === 1, 'failed checkout API -> signup_error ' + JSON.stringify(er));
ok(checkoutBody && 'ph' in checkoutBody && 'ab' in checkoutBody && checkoutBody.plan === 'solo', 'checkout body carries ph and ab for server-side attribution ' + JSON.stringify(checkoutBody && { ph: checkoutBody.ph, ab: checkoutBody.ab }));
ok(p.errs.length === 0, 'no JS errors on /start/ ' + p.errs.join('|'));
// EXP-157: no exit above the form, and the IP-lock hint does not toggle the box.
ok(await p.locator('.site-header a[href="/#get"]').count() === 0, '/start/ header has no Get started link away from the form');
ok(await p.evaluate(() => { const f = document.getElementById('f'), n = document.querySelector('.free-note[data-cta-loc="start-selfhost"]'); return !!n && !!(f.compareDocumentPosition(n) & Node.DOCUMENT_POSITION_FOLLOWING); }), 'self-host note sits below the form');
await p.click('.field.lock .hint');
ok(!(await p.isChecked('#iplock')), 'clicking the IP-lock hint leaves the box unchecked');
await p.click('label[for="iplock"]');
ok(await p.isChecked('#iplock'), 'clicking the IP-lock label checks the box');

// Concierge links: /start/ (outside the form, so no form start), /pricing/ and a solutions page
p = await page({ mobile: true });
await p.goto(B + '/start/');
await p.evaluate(() => document.addEventListener('click', e => e.preventDefault()));
await p.click('[data-cta-loc="start-concierge"] a');
cta = await evs(p, 'cta_click');
ok(cta.length === 1 && cta[0].goal === 'concierge_page' && cta[0].spot === 'start-concierge' && cta[0].href === '/concierge/?source=start', '/start/ Concierge line -> goal concierge_page, spot start-concierge ' + JSON.stringify(cta));
ok((await evs(p, 'signup_form_start')).length === 0, 'clicking the Concierge line sends no signup_form_start');
ok(await p.evaluate(() => document.documentElement.scrollWidth <= innerWidth), '/start/ has no sideways overflow at 390');
p = await page();
await p.goto(B + '/pricing/');
await p.evaluate(() => document.addEventListener('click', e => e.preventDefault()));
await p.click('[data-cta-loc="pricing-concierge"] a.pricing-cta');
cta = await evs(p, 'cta_click');
ok(cta.length === 1 && cta[0].goal === 'concierge_page' && cta[0].spot === 'pricing-concierge', 'pricing Concierge card -> goal concierge_page ' + JSON.stringify(cta));
p = await page();
await p.goto(B + '/solutions/skilled-nursing-facilities/');
await p.evaluate(() => document.addEventListener('click', e => e.preventDefault()));
await p.click('[data-cta-loc="sol-top"] a');
cta = await evs(p, 'cta_click');
ok(cta.length === 1 && cta[0].goal === 'concierge_page' && cta[0].spot === 'sol-top' && cta[0].href === '/concierge/?source=solutions-skilled-nursing-facilities', 'solutions top button -> goal concierge_page ' + JSON.stringify(cta));
ok(p.errs.length === 0, 'no JS errors on a solutions page ' + p.errs.join('|'));

// Header Solutions menu: a <details> that opens by click, tap and keyboard on every page,
// closes on Escape and outside click, and tags each industry link.
for (const mobile of [false, true]) {
  p = await page({ mobile });
  await p.goto(B + '/guides/harness-engineering/');
  await p.evaluate(() => document.addEventListener('click', e => { if (e.target.closest('.nav-drop-menu a')) e.preventDefault(); }));
  const w = mobile ? '390' : '1440';
  const sum = p.locator('.site-header .nav-drop > summary'), menu = p.locator('.nav-drop-menu');
  ok(await sum.isVisible(), `${w}: Solutions menu button is in the header`);
  await sum.click();
  await p.waitForFunction(() => document.querySelector('.nav-drop > summary').getAttribute('aria-expanded') === 'true', null, { timeout: 2000 }).catch(() => {});
  ok(await menu.isVisible() && await sum.getAttribute('aria-expanded') === 'true', `${w}: click opens it, aria-expanded true`);
  ok(await p.evaluate(() => { const m = document.querySelector('.nav-drop-menu').getBoundingClientRect(); return m.left >= 0 && m.right <= innerWidth && document.documentElement.scrollWidth <= innerWidth; }), `${w}: open menu fits the screen`);
  ok((await p.$$eval('.nav-drop-menu a[href^="/solutions/"]', a => a.length)) === 6, `${w}: five industries plus All solutions`);
  await p.keyboard.press('Escape');
  await p.waitForTimeout(50);
  ok(!(await menu.isVisible()) && await sum.getAttribute('aria-expanded') === 'false', `${w}: Escape closes it`);
  await sum.focus(); await p.keyboard.press('Enter');
  ok(await menu.isVisible(), `${w}: Enter opens it`);
  await p.mouse.click(10, 600);
  ok(!(await menu.isVisible()), `${w}: a click elsewhere closes it`);
  await sum.click();
  await p.click('.nav-drop-menu a[href="/solutions/accounting-firms/"]');
  cta = await evs(p, 'cta_click');
  ok(cta.length === 1 && cta[0].goal === 'solutions' && cta[0].spot === 'nav-solutions-accounting-firms', `${w}: industry link -> goal solutions, spot nav-solutions-accounting-firms ` + JSON.stringify(cta));
  if (mobile) ok(await p.locator('.nav-drop-menu .nd-more a[href="/pricing/"]').isVisible(), '390: the menu carries the links the phone header hides');
  ok(p.errs.length === 0, `${w}: no JS errors with the menu ` + p.errs.join('|'));
}
// With an old cached site.js (or none), the menu still opens: <details> needs no script.
p = await page();
await p.route(/\/site\.js(\?|$)/, r => r.fulfill({ contentType: 'application/javascript', body: '' }));
await p.goto(B + '/pricing/');
await p.click('.site-header .nav-drop > summary');
ok(await p.locator('.nav-drop-menu').isVisible(), 'Solutions menu opens with no site.js');

// Welcome page: paid once per order
p = await page();
await p.goto(B + '/start/welcome/?session_id=cs_test_funnel');
ok((await evs(p, 'signup_paid')).length === 1, 'welcome -> signup_paid');
await p.reload();
ok((await evs(p, 'signup_paid')).length === 0, 'reload does not re-send signup_paid');
p = await page();
await p.goto(B + '/start/welcome/?session_id=cs_test_2&tk=x');
const tp = await evs(p, 'signup_paid');
ok(tp.length === 1 && tp[0].test === true, 'test checkout is flagged test:true');
// A/B: a test registered in site.js splits visitors at random, hides the other variant before
// paint, keeps the visitor in their variant, reports exposure, and rides into the checkout.
async function abPage(ctx) {
  const p = ctx ? await ctx.newPage() : await page();
  if (!ctx) await p.route(/posthog\.com|cloud\.amux\.io/, r => r.abort());
  await p.route(/\/site\.js(\?|$)/, async r => {
    const body = (await (await r.fetch()).text()).replace('var AB_TESTS = {', "var AB_TESTS = { exptest: { variants: ['control', 'b'], pages: /^\\/$/ },");
    r.fulfill({ contentType: 'application/javascript', body });
  });
  await p.route(B + '/', async r => {
    const body = (await (await r.fetch()).text()).replace('<div class="hero" data-cta-loc="hero">',
      '<div class="hero" data-cta-loc="hero"><span id="ab-c" data-ab="exptest:control">C</span><span id="ab-b" data-ab="exptest:b">B</span>');
    r.fulfill({ contentType: 'text/html', body });
  });
  return p;
}
const seen = new Set();
for (let i = 0; i < 12; i++) {
  p = await abPage();
  await p.goto(B + '/');
  const v = await p.evaluate(() => document.documentElement.getAttribute('data-ab-exptest'));
  const shown = await p.evaluate(() => ['ab-c', 'ab-b'].filter(id => getComputedStyle(document.getElementById(id)).display !== 'none'));
  if (i === 0) {
    ok(shown.length === 1 && shown[0] === (v === 'control' ? 'ab-c' : 'ab-b'), 'only the assigned variant is visible (' + v + ')');
    const ex = await evs(p, 'ab_exposure');
    ok(ex.length === 1 && ex[0].experiment === 'exptest' && ex[0].variant === v, 'ab_exposure sent with the variant');
    const reg = (await calls(p)).find(c => c[0] === 'register' && c[1] && c[1].ab_exptest);
    ok(reg && reg[1].ab_exptest === v && reg[1]['$feature/exptest'] === v, 'ab_exptest registered on every later event');
    await p.reload();
    ok(await p.evaluate(() => document.documentElement.getAttribute('data-ab-exptest')) === v, 'variant sticks across reloads');
  }
  seen.add(v);
  await p.context().close();
}
ok(seen.size === 2, 'twelve new visitors land in both variants ' + [...seen].join(','));
p = await abPage();
await p.goto(B + '/pricing/');
ok((await evs(p, 'ab_exposure')).length === 0, 'a page outside the test sends no exposure');
const stale = await p.evaluate(() => { localStorage.setItem('amux-ab', JSON.stringify({ exptest: 'b', oldtest: 'x' })); return 1; });
await p.reload();
ok(await p.evaluate(() => !('oldtest' in JSON.parse(localStorage.getItem('amux-ab')))), 'ended tests are dropped from storage');

await b.close();
console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
