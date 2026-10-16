#!/usr/bin/env node
// END-TO-END: vault secrets (AMUX-5375). Scoped key/value items, encrypted at
// rest, delivered into a worker's environment at launch, never readable back.
// Private server, private tmux, the fake `claude` on PATH reporting only the
// SHA-256 of each named variable, so no log in this test holds a value either.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/vault-secrets.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const V = {
  global: 'vt-global-7f3a91', group: 'vt-group-2c88d0', tokGlobal: 'vt-tokglobal-51e2',
  tokWorker: 'vt-tokworker-a94b', plain: 'vt-plaintext-0d1e', replaced: 'vt-replaced-66c3',
  imported: 'vt-imported-3b7f', ui: 'vt-ui-typed-9e21',
};
const sha = s => crypto.createHash('sha256').update(s).digest('hex');
const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const KEYS = 'VT_TOKEN,VT_GROUP,VT_GLOBAL';
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY,
  env: { AMUX_VAULT_KEYSTORE: 'file', FAKE_CLAUDE_ENV_KEYS: KEYS } });
// The owner is the request that presents the owner bearer (what the dashboard
// sends). A worker origin carries the bearer too here, to prove the worker
// header alone is what gets it refused.
const OWNER = { Authorization: 'Bearer ' + fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim() };
const W = name => ({ ...OWNER, 'X-Amux-Session': name });
const call = async (m, p, b, h = OWNER) => { const r = await amux.req(m, p, b, 60000, h); return { ...r, raw: JSON.stringify(r.body ?? '') }; };
const launches = cwd => amux.fakeLog().filter(r => r.event === 'launch' && r.cwd && fs.realpathSync(r.cwd) === fs.realpathSync(cwd));
async function launchOf(name, dir, n) {
  await waitFor(`${name} launch #${n}`, () => launches(dir).length >= n, 60000);
  return launches(dir)[n - 1].env_sha256 || {};
}
try {
  const d1 = path.join(amux.root, 'w1'), d2 = path.join(amux.root, 'w2');
  fs.mkdirSync(d1); fs.mkdirSync(d2);
  for (const [name, dir, tags] of [['vw1', d1, ['ops']], ['vw2', d2, []]]) {
    const c = await call('POST', '/api/sessions', { name, dir, tags });
    check(`worker ${name} created`, c.status < 300, c.body);
  }
  // Owner (no worker header) adds four items across all three scopes.
  const add = async (key, scope, target, value) => call('POST', '/api/vault/secrets', { key, scope, target, value });
  const a = [await add('VT_GLOBAL', 'global', '', V.global), await add('VT_GROUP', 'group', 'ops', V.group),
    await add('VT_TOKEN', 'global', '', V.tokGlobal), await add('VT_TOKEN', 'worker', 'vw1', V.tokWorker)];
  check('owner adds items at global, group and worker scope', a.every(r => r.status === 201), a.map(r => r.status));
  check('no create response carries a value', a.every(r => !Object.values(V).some(v => r.raw.includes(v))));
  const dup = await add('VT_TOKEN', 'global', '', 'x');
  check('a second value for the same key and scope is refused', dup.status === 409, dup.body);
  const harness = await add('AMUX_URL', 'global', '', 'https://evil');
  check('harness variables are refused', harness.status === 400, harness.body);

  // Workers cannot write, and see only what applies to them.
  const id = key => a.find(r => r.body.item?.key === key && r.body.item.scope === 'worker')?.body.item.id
    || a.find(r => r.body.item?.key === key)?.body.item.id;
  const wc = await call('POST', '/api/vault/secrets', { key: 'VT_SNEAK', scope: 'global', value: 'x' }, W('vw1'));
  const wr = await call('PUT', `/api/vault/secrets/${id('VT_GLOBAL')}`, { value: 'x' }, W('vw1'));
  const wd = await call('DELETE', `/api/vault/secrets/${id('VT_GLOBAL')}`, undefined, W('vw1'));
  check('a worker cannot add, replace or delete', [wc, wr, wd].every(r => r.status === 403), [wc.status, wr.status, wd.status]);
  const nb = await call('POST', '/api/vault/secrets', { key: 'VT_NOBEARER', scope: 'global', value: 'x1234' }, {});
  check('a request without the owner bearer cannot write', nb.status === 403, nb.body);
  const l2 = await call('GET', '/api/vault/secrets', undefined, W('vw2'));
  const seen = (l2.body.items || []).map(i => `${i.key}@${i.scope}`).sort();
  check('vw2 sees only items that apply to it (global)', JSON.stringify(seen) === JSON.stringify(['VT_GLOBAL@global', 'VT_TOKEN@global']), seen);
  const l1 = await call('GET', '/api/vault/secrets?worker=vw1');
  const res = Object.fromEntries((l1.body.resolved || []).map(x => [x.key, `${x.scope}:${x.target}`]));
  check('resolution for vw1: worker > group > global', res.VT_TOKEN === 'worker:vw1' && res.VT_GROUP === 'group:ops' && res.VT_GLOBAL === 'global:', res);
  check('no list response carries a value', ![l1, l2].some(r => Object.values(V).some(v => r.raw.includes(v))));

  // A plaintext line for the same key, as mid-migration: the vault must win.
  fs.appendFileSync(path.join(amux.home, 'sessions', 'vw1.env'), `VT_TOKEN="${V.plain}"\n`);

  // Launch both workers and read what their processes actually received.
  for (const n of ['vw1', 'vw2']) await call('POST', `/api/sessions/${n}/start`);
  const e1 = await launchOf('vw1', d1, 1), e2 = await launchOf('vw2', d2, 1);
  check('vw1 gets its worker value, not global and not the plaintext line', e1.VT_TOKEN === sha(V.tokWorker), e1);
  check('vw1 gets its group value and the global one', e1.VT_GROUP === sha(V.group) && e1.VT_GLOBAL === sha(V.global), e1);
  check('vw2 gets the global token and no group value', e2.VT_TOKEN === sha(V.tokGlobal) && e2.VT_GROUP === null, e2);

  // Replace one value and delete another, then restart vw1 in its surviving
  // tmux session: the new value arrives and the deleted key is gone.
  const rep = await call('PUT', `/api/vault/secrets/${id('VT_GLOBAL')}`, { value: V.replaced });
  const del = await call('DELETE', `/api/vault/secrets/${id('VT_GROUP')}`);
  check('owner replaces and deletes', rep.status === 200 && del.status === 200 && !rep.raw.includes(V.replaced), [rep.status, del.status]);
  await call('POST', '/api/sessions/vw1/stop');
  await new Promise(r => setTimeout(r, 1500));
  await call('POST', '/api/sessions/vw1/start');
  const e1b = await launchOf('vw1', d1, 2);
  check('after restart: replaced value delivered, deleted key unset', e1b.VT_GLOBAL === sha(V.replaced) && e1b.VT_GROUP === null && e1b.VT_TOKEN === sha(V.tokWorker), e1b);

  // Opt-in import copies a plaintext value in and leaves the file alone.
  const amuxEnv = path.join(amux.home, 'amux.env');
  fs.appendFileSync(amuxEnv, `VT_IMPORT="${V.imported}"\n`);
  const before = fs.readFileSync(amuxEnv, 'utf8');
  const imp = await call('POST', '/api/vault/secrets/import', { key: 'VT_IMPORT', scope: 'global' });
  check('import stores the value and names the file it came from', imp.status === 201 && imp.body.still_in === amuxEnv && !imp.raw.includes(V.imported), imp.body);
  check('import does not touch the plaintext file', fs.readFileSync(amuxEnv, 'utf8') === before);

  // The owner adds one on the Vault page, at phone width, and never sees a value.
  {
    const br = await chromium.launch();
    const pg = await (await br.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
    await pg.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await pg.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
    await pg.waitForFunction(() => typeof openVault === 'function');
    await pg.evaluate(() => openVault('secrets'));
    await pg.locator('.vault-row').first().waitFor({ timeout: 10000 });
    const listed = await pg.locator('#vault-secrets-table').innerText();
    check('the Vault page lists keys and scopes', /VT_TOKEN/.test(listed) && /vw1/.test(listed) && /Global/.test(listed), listed.slice(0, 300));
    await pg.locator('#vault-add-btn').click();
    await pg.fill('#vault-e-key', 'VT_UI');
    await pg.check('input[name="vault-e-scope"][value="worker"]');
    await pg.selectOption('#vault-e-worker', 'vw2');
    await pg.fill('#vault-e-value', V.ui);
    const btn = await pg.locator('#vault-e-save').boundingBox();
    await pg.locator('#vault-e-save').click();
    await pg.locator('.vault-row', { hasText: 'VT_UI' }).waitFor({ timeout: 10000 });
    const after = await pg.evaluate(() => ({ dialog: !!document.getElementById('vault-edit'), text: document.body.innerText, html: document.body.innerHTML }));
    check('the dialog closes (and its value with it) after saving', !after.dialog);
    check('no value appears anywhere on the page', !Object.values(V).some(v => after.text.includes(v) || after.html.includes(v)));
    check('the save button is a 44px touch target on a phone', btn && btn.height >= 44, btn);
    await pg.screenshot({ path: path.join(amux.root, 'vault-secrets-page.png'), fullPage: false });
    await br.close();
    const ui = (await call('GET', '/api/vault/secrets?worker=vw2')).body.resolved || [];
    check('the item added on the Vault page resolves for vw2', ui.some(x => x.key === 'VT_UI' && x.scope === 'worker'), ui);
  }

  // Leak sweep: nowhere a value could land.
  const read = f => { try { return fs.readFileSync(f, 'utf8'); } catch { return ''; } };
  const leaks = [];
  const scan = (label, text) => { for (const [k, v] of Object.entries(V)) if (k !== 'plain' && k !== 'imported' && text.includes(v)) leaks.push(`${label}:${k}`); };
  scan('server.log', read(amux.serverLog));
  scan('vault-audit', read(path.join(amux.home, 'logs', 'vault-audit.jsonl')));
  scan('secrets.json', read(path.join(amux.home, 'vault', 'secrets.json')));
  scan('fake-claude.log', read(path.join(amux.root, 'fake-claude.log')));
  scan('ps argv', execFileSync('ps', ['-axww', '-o', 'command'], { encoding: 'utf8' }));
  for (const n of ['vw1', 'vw2']) { try { scan(`pane ${n}`, amux.tmux('capture-pane', '-p', '-S', '-2000', '-t', `=amux-${n}:`)); } catch {} }
  const db = path.join(amux.home, 'amux.db');
  if (fs.existsSync(db)) scan('amux.db', fs.readFileSync(db).toString('latin1'));
  check('no value in logs, audit, store, argv, pane history or the database', !leaks.length, leaks);
  const secretsTxt = read(path.join(amux.home, 'vault', 'secrets.json'));
  check('the imported value is encrypted at rest too', secretsTxt.length > 0 && !secretsTxt.includes(V.imported));
  for (const f of ['secrets.json', 'master.key']) {
    const st = fs.statSync(path.join(amux.home, 'vault', f));
    check(`${f} is private (0600)`, (st.mode & 0o777) === 0o600, (st.mode & 0o777).toString(8));
  }
  const audit = read(path.join(amux.home, 'logs', 'vault-audit.jsonl'));
  check('audit records add, refusal, delivery, replace, delete and import',
    ['secret_added', 'secret_write_refused', 'secrets_delivered', 'secret_replaced', 'secret_deleted', 'secret_imported'].every(e => audit.includes(`"event":"${e}"`)));
  check('server log carries the delivery and refusal verdicts',
    ['vault_delivered', 'vault_write_refused_worker_origin', 'vault_key_file_backend'].every(v => read(amux.serverLog).includes(v)));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
