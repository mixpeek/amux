#!/usr/bin/env node
// END-TO-END (governance PR 4): a connector token is minted for whoever the
// request proves, never for whoever a header claims or omits.
//
// A private amux (own home, port and tmux socket) holding two FAKE stored
// Google grants (x@ and y@e2e.invalid, fresh, so a mint never leaves the box)
// and three workers:
//   w-scoped   minted token on disk, connectors scope google-gmail -> x@ only
//   w-claimer  minted token on disk, but the request presents none (a forgery)
//   w-new      never minted (a lane that has not relaunched since PR 1)
// Checks:
//   a. the diagnostic (GET /api/contract/counters -> connector_token) reports
//      mode report, measured, enforceable;
//   b. w-scoped with its token gets x@'s bearer and is refused y@ (its scope),
//      in every mode;
//   c. report mode: a headerless mint and a forged worker claim are allowed
//      and counted would_refuse, one log line;
//   d. every mint leaves a receipt (GET /api/policy/receipts) naming the
//      actor and account and never the bearer;
//   e. enforce (with loopback trust in report so the request reaches the
//      mint): headerless and forged-claim mints are 403 connector_token_refused
//      with a how_to_fix; the owner and w-scoped still mint; w-new (unminted)
//      is allowed and counted, not locked out;
//   f. with PR 3's AMUX_LOOPBACK_TRUST=enforce a headerless loopback mint never
//      reaches the mint at all (401);
//   g. AMUX_CONNECTOR_TOKEN_SCOPE=off takes the check away again.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/connector-token-scope.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {startAmux, waitFor} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const X = 'x@e2e.invalid', Y = 'y@e2e.invalid';

// Fake grants, fresh for an hour: the mint serves them without a provider call.
const fam = path.join(amux.home, 'connectors', 'google');
fs.mkdirSync(fam, {recursive: true});
for (const [acct, tok] of [[X, 'FAKE-BEARER-X'], [Y, 'FAKE-BEARER-Y']]) {
  fs.writeFileSync(path.join(fam, acct + '.json'), JSON.stringify({
    token: tok, scopes: 'https://www.googleapis.com/auth/gmail.modify', expires_at: Date.now() / 1000 + 3600,
  }), {mode: 0o600});
}
// Workers: a session env each; minted token hashes for two of them.
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
const tokens = {};
for (const lane of ['w-scoped', 'w-claimer', 'w-new']) {
  fs.writeFileSync(path.join(amux.home, 'sessions', lane + '.env'), 'CC_TAGS=""\nCC_ARCHIVED=0\n');
}
for (const lane of ['w-scoped', 'w-claimer']) {
  tokens[lane] = crypto.randomBytes(24).toString('hex');
  fs.writeFileSync(path.join(amux.home, 'worker-tokens', lane + '.sha256'),
    crypto.createHash('sha256').update(tokens[lane]).digest('hex'), {mode: 0o600});
}
const AS = (lane, withToken = true) => ({'X-Amux-Session': lane, ...(withToken ? {'X-Amux-Worker-Token': tokens[lane]} : {})});
const mint = (acct, headers) => amux.req('POST', `/api/connectors/google-gmail/token?account=${encodeURIComponent(acct)}`, undefined, 10000, headers);
const counters = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.connector_token || {};
const logLines = verdict => fs.readFileSync(amux.serverLog, 'utf8').split('\n').filter(l => l.includes(verdict));
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };

let ok = false;
try {
  // Scope w-scoped to x@ only (owner sets it, as the dashboard does).
  const scoped = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'w-scoped', capability: 'connectors',
    value: {'google-gmail': {enabled: true, account: X}}}, 10000, OWNER());
  check('setup: the owner scopes w-scoped to x@', scoped.status === 200, scoped);

  // a. Diagnostic.
  let c = await counters();
  check('a. report is the default mode, measured, enforceable', c.mode === 'report' && c.measured === true && c.enforceable === true, c);

  // b. The proven worker is held to its scope.
  const bx = await mint(X, AS('w-scoped'));
  check('b. w-scoped with its token gets x@', bx.status === 200 && bx.body.access_token === 'FAKE-BEARER-X', bx);
  const by = await mint(Y, AS('w-scoped'));
  check('b. w-scoped is refused y@ (its scope)', by.status === 403 && by.body.blocked === 'connector_scope' && !by.body.access_token, by);

  // c. Report mode: unproven callers allowed and counted.
  const before = (await counters()).mints;
  const anon = await mint(Y, {});
  check('c. report: a headerless mint still gets a bearer', anon.status === 200 && anon.body.access_token === 'FAKE-BEARER-Y', anon);
  const forged = await mint(X, AS('w-claimer', false));
  check('c. report: a worker claim without its token still mints', forged.status === 200, forged);
  c = await counters();
  check('c. both were counted would_refuse', c.mints.would_refuse >= before.would_refuse + 2, {before, after: c.mints});
  await waitFor('would-refuse log line', () => logLines('connector_token_would_be_refused').length > 0, 10000);
  check('c. one log line for many', logLines('connector_token_would_be_refused').length === 1);

  // d. Receipts.
  const rec = await amux.req('GET', '/api/policy/receipts?limit=1000', undefined, 10000, OWNER());
  const mints = (rec.body.items || []).filter(r => r.action === 'connector_token_mint');
  check('d. every mint so far left a receipt', mints.length >= 4, {n: mints.length, rec: rec.status});
  check('d. the proven worker is named as the actor', mints.some(r => r.actor === 'worker:w-scoped' && r.resource.includes(X) && r.effect === 'allow'), mints);
  check('d. the refused scope mint is recorded as denied', mints.some(r => r.actor === 'worker:w-scoped' && r.effect === 'deny'), mints);
  check('d. the headerless mint is recorded as anonymous', mints.some(r => r.actor === 'anonymous'), mints);
  check('d. no receipt holds a bearer', !JSON.stringify(mints).includes('FAKE-BEARER'), mints);

  // e. Enforce (loopback trust in report so requests reach the mint).
  await restart({AMUX_CONNECTOR_TOKEN_SCOPE: 'enforce', AMUX_LOOPBACK_TRUST: 'report'});
  check('e. restarted in enforce', (await counters()).mode === 'enforce');
  const e1 = await mint(Y, {});
  check('e. a headerless mint is refused with a way forward', e1.status === 403 && e1.body.code === 'connector_token_refused' && !!e1.body.how_to_fix && !e1.body.access_token, e1);
  const e2 = await mint(X, AS('w-claimer', false));
  check('e. a forged worker claim is refused', e2.status === 403 && e2.body.code === 'connector_token_refused', e2);
  const e3 = await mint(Y, OWNER());
  check('e. the owner still mints', e3.status === 200 && e3.body.access_token === 'FAKE-BEARER-Y', e3);
  const e4 = await mint(X, AS('w-scoped'));
  check('e. the proven worker still mints within its scope', e4.status === 200 && e4.body.access_token === 'FAKE-BEARER-X', e4);
  const e5 = await mint(Y, AS('w-scoped'));
  check('e. and is still refused outside it', e5.status === 403 && e5.body.blocked === 'connector_scope', e5);
  const beforeNew = (await counters()).mints;
  const e6 = await mint(X, AS('w-new', false));
  const afterNew = (await counters()).mints;
  check('e. an unminted lane is not locked out, and is counted', e6.status === 200 && afterNew.worker_unminted > beforeNew.worker_unminted, {e6, beforeNew, afterNew});
  await waitFor('refused log line', () => logLines('connector_token_refused').length > 0, 10000);

  // f. PR 3's loopback enforce stops a headerless mint before it is asked.
  await restart({AMUX_LOOPBACK_TRUST: 'enforce'});
  const f1 = await mint(Y, {});
  check('f. loopback enforce: a headerless mint is 401 before the mint', f1.status === 401, f1);
  const f2 = await mint(X, AS('w-scoped'));
  check('f. loopback enforce: the proven worker still mints', f2.status === 200, f2);

  // g. Off.
  await restart({AMUX_CONNECTOR_TOKEN_SCOPE: 'off', AMUX_LOOPBACK_TRUST: 'report'});
  const g1 = await mint(Y, {});
  check('g. AMUX_CONNECTOR_TOKEN_SCOPE=off checks nothing', g1.status === 200 && (await counters()).mode === 'off', g1);
  ok = true;
} finally {
  const receipt = {scenario: 'connector-token-scope', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
