/**
 * Full UI E2E test: invitation cards on /create-organization
 * Uses PropelAuth magic link to authenticate a fresh user with no org,
 * then verifies the invitation UI appears and can be accepted.
 */

const { chromium } = require('playwright');
const https = require('https');
const http = require('http');

const PROPELAUTH_URL = 'https://698686699.propelauthtest.com';
const PROPELAUTH_API_KEY = '22db09cac845333028eb3b289a7964a167fe81a152e6412944a748d34616f0d42549e87fccff68ec2b565c168b81aa06';
const MIXPEEK_PRIVATE_TOKEN = 'xnefritAiaKQiddNL3ZHWEN4cHWLsCkwEycUDLU2wLekQEuf';
const BACKEND = 'http://localhost:8000';
const FRONTEND = 'http://localhost:8080';

const ts = Date.now();
const TEST_EMAIL = `inviteui_${ts}@mailtest.com`;
const TEST_ORG_NAME = `InviteUITest${ts}`;

async function req(url, opts = {}) {
  return new Promise((resolve, reject) => {
    const isHttps = url.startsWith('https');
    const lib = isHttps ? https : http;
    const u = new URL(url);
    const options = {
      method: opts.method || 'GET',
      hostname: u.hostname,
      port: u.port || (isHttps ? 443 : 80),
      path: u.pathname + u.search,
      headers: { 'Content-Type': 'application/json', ...(opts.headers || {}) },
      rejectUnauthorized: false,
    };
    const r = lib.request(options, (res) => {
      let d = '';
      res.on('data', (c) => { d += c; });
      res.on('end', () => {
        try { resolve({ status: res.statusCode, body: JSON.parse(d) }); }
        catch { resolve({ status: res.statusCode, body: d }); }
      });
    });
    r.on('error', reject);
    if (opts.body) r.write(JSON.stringify(opts.body));
    r.end();
  });
}

async function main() {
  console.log('=== Invitation UI Full E2E Test ===');
  console.log(`Email: ${TEST_EMAIL}`);
  console.log(`Org:   ${TEST_ORG_NAME}\n`);

  const ph = { Authorization: `Bearer ${PROPELAUTH_API_KEY}` };
  const mx = { Authorization: `Bearer ${MIXPEEK_PRIVATE_TOKEN}` };

  // 1. Create PropelAuth user
  console.log('1. Creating PropelAuth user...');
  const userRes = await req(`${PROPELAUTH_URL}/api/backend/v1/user/`, {
    method: 'POST', headers: ph,
    body: { email: TEST_EMAIL, email_confirmed: true, send_email_to_confirm_email_address: false },
  });
  if (userRes.status !== 200 && userRes.status !== 201) {
    console.error('Failed:', userRes.status, userRes.body);
    process.exit(1);
  }
  const propelUserId = userRes.body.user_id;
  console.log(`   ✅ PropelAuth user: ${propelUserId}`);

  // 2. Create Mixpeek test org
  console.log('2. Creating Mixpeek org...');
  const orgRes = await req(`${BACKEND}/v1/private/organizations`, {
    method: 'POST', headers: mx,
    body: {
      organization_name: TEST_ORG_NAME,
      users: [{ email: 'admin@testorg.com', user_name: 'Admin', role: 'admin' }],
    },
  });
  if (orgRes.status !== 200 && orgRes.status !== 201) {
    console.error('Failed:', orgRes.status, orgRes.body);
    process.exit(1);
  }
  const orgId = orgRes.body.organization_id;
  console.log(`   ✅ Org: ${orgId}`);

  // 3. Add test user as pending invitee
  console.log('3. Adding pending invitation...');
  const addRes = await req(`${BACKEND}/v1/private/organizations/add-user`, {
    method: 'POST', headers: mx,
    body: {
      organization_identifier: orgId,
      users: [{ email: TEST_EMAIL, user_name: 'Test User', role: 'member', status: 'pending' }],
    },
  });
  console.log(`   ${addRes.status === 200 ? '✅' : '❌'} Add user: ${addRes.status}`);

  // 4. Get magic login link (no trailing slash!)
  console.log('4. Getting magic login link...');
  const mlRes = await req(`${PROPELAUTH_URL}/api/backend/v1/magic_link`, {
    method: 'POST', headers: ph,
    body: {
      email: TEST_EMAIL,
      redirect_to_url: `${FRONTEND}/create-organization`,
      create_if_doesnt_exist: false,
    },
  });
  if (mlRes.status !== 200 || !mlRes.body.url) {
    console.error('Magic link failed:', mlRes.status, mlRes.body);
    process.exit(1);
  }
  const magicUrl = mlRes.body.url;
  console.log(`   ✅ Magic link obtained`);

  // 5. Launch Playwright and authenticate via magic link
  console.log('5. Launching browser...');
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    ignoreHTTPSErrors: true,
  });
  const page = await ctx.newPage();

  let pass = [];
  let fail = [];

  try {
    // Navigate to magic link
    console.log('   Navigating to magic link...');
    await page.goto(magicUrl, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await page.waitForTimeout(4000);

    let url = page.url();
    console.log(`   URL after magic link: ${url}`);
    await page.screenshot({ path: '/tmp/pw-final-01-after-magic.png' });

    // Wait up to 10s for redirect to complete
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      url = page.url();
      if (url.includes(FRONTEND)) break;
      await page.waitForTimeout(500);
    }

    console.log(`   URL after wait: ${url}`);
    await page.screenshot({ path: '/tmp/pw-final-02-frontend.png' });

    // Handle "Complete your profile" page (PropelAuth first-login step)
    const bodyNow = await page.evaluate(() => document.body.innerText);
    if (bodyNow.includes('Complete your profile') || bodyNow.includes('First name')) {
      console.log('   Filling in profile form (first/last name)...');
      await page.fill('input[placeholder="First name"], input[name="firstName"], input[id*="first"]', 'Test');
      await page.fill('input[placeholder="Last name"], input[name="lastName"], input[id*="last"]', 'Invitee');
      await page.screenshot({ path: '/tmp/pw-final-02b-profile.png' });
      await page.click('button:has-text("Continue")');
      await page.waitForTimeout(4000);
      url = page.url();
      console.log(`   URL after profile submit: ${url}`);
      await page.screenshot({ path: '/tmp/pw-final-02c-after-profile.png' });
    }

    // If not yet on /create-organization, navigate there explicitly
    if (!url.includes('/create-organization')) {
      console.log(`   Navigating to /create-organization...`);
      await page.goto(`${FRONTEND}/create-organization`, {
        waitUntil: 'domcontentloaded',
        timeout: 15000,
      });
      await page.waitForTimeout(3000);
      url = page.url();
      console.log(`   URL: ${url}`);
      await page.screenshot({ path: '/tmp/pw-final-02d-create-org-nav.png' });
    }

    // If landed on /create-organization, check for invitation UI
    if (url.includes('/create-organization')) {
      console.log('\n6. Checking /create-organization for invitation UI...');
      // Wait a bit more for React to load and useEffect to fire
      await page.waitForTimeout(3000);
      await page.screenshot({ path: '/tmp/pw-final-03-create-org.png' });

      const bodyText = await page.evaluate(() => document.body.innerText);
      console.log('   Page text (first 800 chars):');
      console.log('   ' + bodyText.slice(0, 800).replace(/\n/g, '\n   '));

      const hasOrgName = bodyText.includes(TEST_ORG_NAME);
      const hasJoin = bodyText.toLowerCase().includes('join');
      const hasInvite = bodyText.toLowerCase().includes('invitation') ||
                        bodyText.toLowerCase().includes('invited');
      const hasCreate = bodyText.toLowerCase().includes('create') &&
                        (bodyText.toLowerCase().includes('workspace') || bodyText.toLowerCase().includes('organization'));

      console.log('\n   ── UI Elements ──');
      console.log(`   Org name "${TEST_ORG_NAME.slice(0, 20)}...": ${hasOrgName ? '✅ YES' : '❌ NO'}`);
      console.log(`   "Join" button/text:   ${hasJoin ? '✅ YES' : '❌ NO'}`);
      console.log(`   Invitation text:      ${hasInvite ? '✅ YES' : '❌ NO'}`);
      console.log(`   Create form:          ${hasCreate ? '✅ YES' : '❌ NO'}`);

      if (hasOrgName || hasJoin) {
        pass.push('Invitation UI visible');
      } else {
        fail.push('Invitation UI not found — may need the API call to return');
        // Check if the API call was made
        const netReqs = [];
        page.on('request', r => { if (r.url().includes('user-invitations')) netReqs.push(r.url()); });
        await page.waitForTimeout(1000);
        console.log('   Network requests for user-invitations:', netReqs);
      }

      if (hasCreate) pass.push('Create workspace form visible');

      // Try to click "Join" if present
      const joinBtn = await page.$('button:has-text("Join")');
      if (joinBtn) {
        console.log('\n7. Clicking "Join" button...');
        await joinBtn.click();
        await page.waitForTimeout(4000);
        await page.screenshot({ path: '/tmp/pw-final-04-after-join.png' });
        const afterUrl = page.url();
        console.log(`   URL after join: ${afterUrl}`);
        const afterText = await page.evaluate(() => document.body.innerText.slice(0, 200));
        console.log(`   Page text: ${afterText}`);
        if (!afterUrl.includes('/create-organization')) {
          pass.push('Join redirected away from create-organization');
        }
      }

    } else if (url.includes('/dashboard') || url.includes('/settings') || url.includes('/app')) {
      console.log(`\n   ℹ️  Redirected to: ${url}`);
      console.log('   (User may have been auto-added to an org by PropelAuth)');
      pass.push('Auth succeeded, redirected to app');
    } else {
      console.log(`\n   Final URL: ${url}`);
      const text = await page.evaluate(() => document.body.innerText.slice(0, 400));
      console.log('   Page:', text);
    }

  } finally {
    await browser.close();
  }

  // Results
  console.log('\n=== Results ===');
  pass.forEach(p => console.log(`✅ ${p}`));
  fail.forEach(f => console.log(`❌ ${f}`));
  console.log('\nScreenshots:');
  [1, 2, 3, 4].forEach(n => console.log(`  /tmp/pw-final-0${n}-*.png`));
}

main().catch(err => {
  console.error('Error:', err.message);
  process.exit(1);
});
