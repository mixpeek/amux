#!/usr/bin/env node
// END-TO-END (governance PR 18): connector grants are encrypted at rest.
//
// A private amux (own home, port and tmux socket) holding FAKE grants only:
// a Google family grant for x@ (fresh, so a mint never leaves the box) and a
// legacy Gmail-mirror token for m@. Checks:
//   a. grants written plain before boot are sealed by the boot pass, and no
//      plaintext refresh-token byte remains anywhere under the home;
//   b. the connector token mint (PR 4) still serves the right bearer;
//   c. GET /api/security/at-rest: connector grants encrypted "yes", provider
//      file, key colocated with the data (true: the key file is in the home),
//      database honestly "no", measured with n_considered;
//   d. the owner's way back: decrypt is refused while sealing is on, and with
//      AMUX_ENCRYPT_AT_REST=off it rewrites the grants as plain files;
//   e. with AMUX_KEY_PROVIDER=env and a key, the boot pass seals again under
//      that key, and the report says the key is not colocated;
//   f. with the env key removed, grants are LOCKED: the mint answers 503
//      status locked, Gmail health says "locked" (never not_connected), and
//      the report counts them locked;
//   g. with the key back, the mint works again.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/encryption-at-rest.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {startAmux} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', AMUX_VAULT_KEYSTORE: 'file',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const X = 'x@e2e.invalid', M = 'm@e2e.invalid';
const SECRET = 'FAKE-REFRESH-SECRET-' + crypto.randomBytes(6).toString('hex');
const GMAIL_DIR = 'gmail' + '-tokens';
const famFile = path.join(amux.home, 'connectors', 'google', X + '.json');
const mirrorFile = path.join(amux.home, GMAIL_DIR, M + '.json');
const restart = async (env, drop = []) => {
  await amux.down();
  for (const k of drop) delete amux.env[k];
  Object.assign(amux.env, env);
  await amux.up();
};
const writePlain = () => {
  fs.mkdirSync(path.dirname(famFile), {recursive: true});
  fs.mkdirSync(path.dirname(mirrorFile), {recursive: true});
  fs.writeFileSync(famFile, JSON.stringify({token: 'FAKE-BEARER-X', refresh_token: SECRET,
    scopes: 'https://www.googleapis.com/auth/gmail.modify', expires_at: Date.now() / 1000 + 3600}), {mode: 0o600});
  fs.writeFileSync(mirrorFile, JSON.stringify({token: 'FAKE-MIRROR', refresh_token: SECRET,
    client_id: 'cid', client_secret: 'csec'}), {mode: 0o600});
};
// Every file under the home, as bytes, looking for the secret.
const plaintextHits = () => {
  const hits = [];
  const walk = dir => {
    for (const e of fs.readdirSync(dir, {withFileTypes: true})) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.isFile()) { try { if (fs.readFileSync(p).includes(SECRET)) hits.push(p); } catch {} }
    }
  };
  walk(amux.home);
  return hits;
};
const sealed = f => fs.readFileSync(f, 'utf8').includes('"amux_at_rest"');
const mint = () => amux.req('POST', `/api/connectors/google-gmail/token?account=${encodeURIComponent(X)}`, undefined, 10000, OWNER());
const atRest = async () => (await amux.req('GET', '/api/security/at-rest', undefined, 20000, OWNER())).body;

let ok = false;
try {
  // a. Plain grants on disk, then a restart: the boot pass seals them.
  writePlain();
  await restart({});
  check('a. the boot pass sealed both grants', sealed(famFile) && sealed(mirrorFile), {fam: fs.readFileSync(famFile, 'utf8').slice(0, 80)});
  check('a. no plaintext refresh token anywhere under the home', plaintextHits().length === 0, plaintextHits());

  // b. The mint still works through the sealed grant.
  const b1 = await mint();
  check('b. the connector token mint serves the bearer from the sealed grant', b1.status === 200 && b1.body.access_token === 'FAKE-BEARER-X', b1);

  // c. The report.
  const c = await atRest();
  check('c. the report is measured and counts the grants', c.measured === true && c.n_considered >= 2, c);
  check('c. connector grants: encrypted yes, nothing plain or locked',
    c.stores.connector_grants.encrypted === 'yes' && c.stores.connector_grants.plain === 0 && c.stores.connector_grants.locked === 0, c.stores);
  check('c. file provider: the key is colocated with the data, said plainly', c.key.provider === 'file' && c.key.colocated_with_data === true, c.key);
  check('c. the database is honestly reported unencrypted', c.stores.database.encrypted === 'no', c.stores.database);
  const workerSees = await amux.req('GET', '/api/security/at-rest', undefined, 10000, {'X-Amux-Session': 'someone'});
  check('c. the report is for people only', workerSees.status === 403, workerSees);

  // d. The owner's way back.
  const d1 = await amux.req('POST', '/api/security/at-rest/decrypt', undefined, 10000, OWNER());
  check('d. decrypt is refused while sealing is on', d1.status === 409, d1);
  await restart({AMUX_ENCRYPT_AT_REST: 'off'});
  const d2 = await amux.req('POST', '/api/security/at-rest/decrypt', undefined, 20000, OWNER());
  check('d. with sealing off the owner rewrites the grants as plain files',
    d2.status === 200 && d2.body.decrypted === 2 && !sealed(famFile) && !sealed(mirrorFile), {d2, fam: fs.readFileSync(famFile, 'utf8').slice(0, 60)});
  const d3 = await mint();
  check('d. the plain grant still mints', d3.status === 200 && d3.body.access_token === 'FAKE-BEARER-X', d3);

  // e. The env provider: a key the home does not hold.
  const KEY = crypto.randomBytes(32).toString('hex');
  await restart({AMUX_ENCRYPT_AT_REST: 'on', AMUX_KEY_PROVIDER: 'env', AMUX_AT_REST_KEY: KEY});
  check('e. the boot pass sealed again under the env key', sealed(famFile) && sealed(mirrorFile) && plaintextHits().length === 0, plaintextHits());
  const e1 = await atRest();
  check('e. env provider: the key is not colocated with the data', e1.key.provider === 'env' && e1.key.colocated_with_data === false && e1.key.available === true, e1.key);
  const e2 = await mint();
  check('e. the mint works under the env key', e2.status === 200 && e2.body.access_token === 'FAKE-BEARER-X', e2);

  // f. Key removed: locked, never "disconnected".
  await restart({}, ['AMUX_AT_REST_KEY']);
  const f1 = await mint();
  check('f. without the key the mint answers locked (503), not needs_auth', f1.status === 503 && f1.body.status === 'locked', f1);
  const f2 = await amux.req('GET', '/api/gmail/accounts', undefined, 20000, OWNER());
  check('f. the Gmail mirror account is still listed and its health is "locked"',
    (f2.body.accounts || []).includes(M) && f2.body.health && f2.body.health[M] === 'locked', f2.body);
  const f3 = await atRest();
  check('f. the report counts both grants locked and the key unavailable',
    f3.stores.connector_grants.locked === 2 && f3.key.available === false, {grants: f3.stores.connector_grants, key: f3.key});
  check('f. the sealed files are untouched (still sealed, no plaintext)', sealed(famFile) && plaintextHits().length === 0, plaintextHits());

  // g. Key back.
  await restart({AMUX_AT_REST_KEY: KEY});
  const g1 = await mint();
  check('g. with the key back the mint works again', g1.status === 200 && g1.body.access_token === 'FAKE-BEARER-X', g1);
  const g2 = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.at_rest || {};
  check('g. the at_rest counters are in the contract diagnostic', g2.measured === true && g2.provider === 'env', g2);
  ok = true;
} finally {
  const receipt = {scenario: 'encryption-at-rest', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
