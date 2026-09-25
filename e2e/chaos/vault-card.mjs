#!/usr/bin/env node
// END-TO-END: a vault card is USED by a worker without being READ, under its
// spending rule (no purchase over $100 without the owner's explicit approval).
// Private server, private Chrome, a local checkout page, a TEST card number.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/vault-card.mjs
import fs from 'node:fs';
import path from 'node:path';
import https from 'node:https';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const TEST_PAN = '5555555555554444';  // Mastercard's public test number
const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
const call = (method, p, body, headers = {}) => new Promise((resolve, reject) => {
  const data = body === undefined ? undefined : Buffer.from(JSON.stringify(body));
  const r = https.request(amux.base + p, { method, rejectUnauthorized: false, timeout: 60000,
    headers: { 'Content-Type': 'application/json', ...headers, ...(data ? { 'Content-Length': data.length } : {}) } }, res => {
    let b = ''; res.on('data', c => b += c); res.on('end', () => { let j = {}; try { j = JSON.parse(b || '{}'); } catch { j = { raw: b }; } resolve({ status: res.statusCode, body: j, raw: b }); });
  });
  r.on('error', reject); if (data) r.write(data); r.end();
});
const W = { 'X-Amux-Session': 'buyer' };   // a worker
try {
  const page = path.join(amux.root, 'checkout.html');
  fs.writeFileSync(page, `<!doctype html><form>
    <input autocomplete="cc-name" name="ccname"><input autocomplete="cc-number" name="cardnumber">
    <input autocomplete="cc-exp" name="exp"><input autocomplete="cc-csc" name="cvc">
    <input autocomplete="postal-code" name="zip"></form>`);
  // Owner adds the card (no worker header).
  const add = await call('POST', '/api/vault', { name: 'Test Mastercard', fields: { number: TEST_PAN, exp: '09/28', cvc: '123', name: 'Test Holder', zip: '10001' } });
  check('owner adds a card: active, rule defaults to $100', add.status === 201 && add.body.item.status === 'active' && add.body.item.rules.max_usd_without_approval === 100, add.body);
  check('the create response carries no card number', !add.raw.includes(TEST_PAN) && !add.raw.includes('"123"'));
  const id = add.body.item.id;
  const list = await call('GET', '/api/vault', undefined, W);
  check('a worker sees only brand, last4 and rules', list.body.items?.[0]?.last4 === '4444' && !list.raw.includes(TEST_PAN), list.body.items?.[0]);
  const wadd = await call('POST', '/api/vault', { name: 'sneaky', fields: { number: TEST_PAN } }, W);
  check('a card added by a worker is not usable until the owner activates it', wadd.body.item?.status === 'pending_owner_activation', wadd.body);
  // The owner activates the worker-staged card from Settings -> Integrations.
  {
    const br = await chromium.launch();
    const pg = await (await br.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
    await pg.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await pg.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
    await pg.waitForFunction(() => typeof _vaultLoad === 'function');
    await pg.evaluate(() => { document.getElementById('settings-menu')?.classList.add('open'); _settingsTab('integrations'); });
    const row = pg.locator(`.vault-item[data-id="${wadd.body.item.id}"]`);
    await row.waitFor({ timeout: 10000 });
    const txt = await row.innerText();
    check('Settings lists the staged card as not active, masked', /not active/.test(txt) && /ending 4444/.test(txt) && !txt.includes(TEST_PAN), txt);
    await row.getByRole('button', { name: 'Activate' }).click();
    await pg.waitForTimeout(1200);
    const after = (await call('GET', '/api/vault')).body.items.find(i => i.id === wadd.body.item.id);
    check('Activate in Settings makes it active', after?.status === 'active', after);
    await pg.screenshot({ path: path.join(amux.root, 'vault-settings.png') });
    await br.close();
  }
  const wrule = await call('PATCH', `/api/vault/${id}`, { rules: { max_usd_without_approval: 100000 } }, W);
  check('a worker cannot raise the limit', wrule.status === 403, wrule.body);

  const pc = await call('POST', '/api/browser/profile/create', { name: 'buyer' }, W);
  check('private browser profile created', pc.status < 300, pc.body);
  const b = await call('POST', '/api/browser/start', { profile: 'buyer', session: 'buyer', url: 'file://' + page }, W);
  check('browser page opened for the worker', b.status < 300, { status: b.status, body: JSON.stringify(b.body).slice(0, 300) });
  await new Promise(r => setTimeout(r, 2000));
  const readPage = async () => (await call('POST', '/api/browser/action', { session: 'buyer', action: 'eval',
    script: 'JSON.stringify([...document.querySelectorAll("input")].map(i=>[i.name,i.value]))' }, W)).body;

  // $40: under the rule, fills.
  const small = await call('POST', `/api/vault/${id}/fill`, { amount_usd: 40, merchant: 'Test Shop', purpose: 'e2e small charge' }, W);
  check('$40: filled without approval', small.status === 200 && small.body.filled?.includes('number'), small.body);
  check('$40: the response never contains the card', !small.raw.includes(TEST_PAN) && !small.raw.includes('"123"'));
  const vals = JSON.stringify(await readPage());
  check('$40: the page really holds the card', vals.includes(TEST_PAN) && vals.includes('09/28') && vals.includes('123'), vals.slice(0, 300));

  // $250: over the rule, needs the owner.
  const big = await call('POST', `/api/vault/${id}/fill`, { amount_usd: 250, merchant: 'Test Shop', purpose: 'e2e big charge' }, W);
  check('$250: refused with a grant to approve', big.status === 403 && big.body.requires_approval === true && /^grn_/.test(big.body.grant_id || ''), big.body);
  const self = await call('POST', `/api/grants/${big.body.grant_id}/approve`, {}, W);
  check('the worker cannot approve its own charge', self.status === 403, self.body);
  const other = await call('POST', `/api/vault/${id}/fill`, { amount_usd: 250, merchant: 'Other Shop', purpose: 'swap merchant' }, W);
  check('an approval cannot be taken before it is given', other.status === 403);
  const ok = await call('POST', `/api/grants/${big.body.grant_id}/approve`, {});
  check('the owner approves it', ok.status === 200 && ok.body.granted === 'vault_spend', ok.body);
  const wrongAmt = await call('POST', `/api/vault/${id}/fill`, { amount_usd: 999, merchant: 'Test Shop', purpose: 'bigger than approved' }, W);
  check('the approval does not cover a different amount', wrongAmt.status === 403 && wrongAmt.body.requires_approval, wrongAmt.body);
  const retry = await call('POST', `/api/vault/${id}/fill`, { amount_usd: 250, merchant: 'Test Shop', purpose: 'e2e big charge' }, W);
  check('the approved $250 charge fills once', retry.status === 200 && retry.body.filled?.includes('number'), retry.body);
  const again = await call('POST', `/api/vault/${id}/fill`, { amount_usd: 250, merchant: 'Test Shop', purpose: 'reuse' }, W);
  check('the same approval cannot be used twice', again.status === 403 && again.body.requires_approval, again.body);

  const auditTxt = fs.readFileSync(path.join(amux.home, 'logs', 'vault-audit.jsonl'), 'utf8');
  check('audit records every decision', ['filled', 'needs_approval'].every(d => auditTxt.includes(`"decision":"${d}"`)));
  check('no card number in the audit, the server log or the vault dir listing', !auditTxt.includes(TEST_PAN) && !fs.readFileSync(amux.serverLog, 'utf8').includes(TEST_PAN));
  const st = fs.statSync(path.join(amux.home, 'vault', 'items.json'));
  check('the store is private (0600)', (st.mode & 0o777) === 0o600, (st.mode & 0o777).toString(8));
  await call('POST', '/api/browser/stop', { session: 'buyer' }, W);
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
