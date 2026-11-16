#!/usr/bin/env node
// END-TO-END (governance PR 2): a request with no proven identity is
// anonymous, never the owner.
//
// A private amux (own home, port and tmux socket) with its owner token on
// disk, exactly as a normal install has. Over loopback, so the auth bypass
// admits every request below and only the new check tells them apart:
//   a. the diagnostic (GET /api/contract/counters -> anonymous_owner) reports
//      mode, measured and whether this server can enforce;
//   b. the owner (the bearer the dashboard sends on every call) activates a
//      vault item and approves a grant, recorded as `owner-token`, verified;
//   c. a worker cannot activate the vault item or approve its own grant;
//   d. a headerless request is let through and counted would_refuse in the
//      default report mode, with one log line per gate;
//   e. after a restart with AMUX_ANONYMOUS_IS_OWNER=enforce the same headerless
//      requests are refused 403 anonymous_refused, a headerless vault card is
//      held for the owner rather than going live, the owner still works, and
//      the shipped `amux` CLI run outside a worker proves itself as the owner;
//   f. AMUX_ANONYMOUS_IS_OWNER=off takes the check away again.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/anonymous-not-owner.mjs
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
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
const WORKER = {'X-Amux-Session': 'e2e-worker'};
const counters = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.anonymous_owner || {};
const grant = (requestedBy) => {
  const id = 'grn_' + [...crypto.getRandomValues(new Uint8Array(12))].map(b => b.toString(16).padStart(2, '0')).join('');
  fs.mkdirSync(path.join(amux.home, 'grants'), {recursive: true});
  fs.writeFileSync(path.join(amux.home, 'grants', id + '.json'), JSON.stringify({
    id, kind: 'vault_spend', requested_by: requestedBy, summary: 'e2e charge',
    payload: {origin: requestedBy, target: 'vault_e2e_100_acme', amount_usd: 1, merchant: 'acme'},
    created: Date.now() / 1000,
  }));
  return id;
};
const vaultItem = (headers, name) => amux.req('POST', '/api/vault', {
  name, kind: 'login', fields: {username: 'e2e@example.invalid', password: 'not-a-real-password'},
  rules: {origins: ['example.invalid']},
}, 10000, headers);
// The shipped CLI from the owner's own terminal: no AMUX_SESSION, no token.
const ownerCli = (...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, CC_HOME: amux.home},
  encoding: 'utf8', timeout: 30000,
});
const logLines = verdict => fs.readFileSync(amux.serverLog, 'utf8').split('\n').filter(l => l.includes(verdict));

let ok = false;
try {
  // a. The diagnostic.
  let c = await counters();
  check('a. report is the default mode', c.mode === 'report', c);
  check('a. the diagnostic says it measured and can enforce here', c.measured === true && c.owner_credential_configured === true && c.enforceable === true, c);

  // b/c. A worker adds a vault item: it waits for the owner.
  const added = await vaultItem(WORKER, 'e2e-login');
  check('a worker-added vault item is pending the owner', added.status === 201 && added.body.item.status === 'pending_owner_activation', added);
  const item = added.body.item.id;
  const byWorker = await amux.req('PATCH', `/api/vault/${item}`, {activate: true}, 10000, WORKER);
  check('c. a worker cannot activate it', byWorker.status === 403, byWorker);

  // d. Report mode: a headerless request still acts, and is counted.
  const before = (await counters()).requests;
  const anon = await amux.req('PATCH', `/api/vault/${item}`, {rules: {max_usd_without_approval: 5}}, 10000, {});
  check('d. report mode lets a headerless vault change through', anon.status === 200, anon);
  c = await counters();
  check('d. it was counted would_refuse', c.requests.would_refuse > before.would_refuse, {before, after: c.requests});
  await waitFor('would-refuse log line', () => logLines('anonymous_would_be_refused').some(l => l.includes('vault_update')), 10000);
  check('d. one log line for the vault gate', logLines('anonymous_would_be_refused').filter(l => l.includes('vault_update')).length === 1);

  // b. The owner activates it, recorded as the owner bearer.
  const byOwner = await amux.req('PATCH', `/api/vault/${item}`, {activate: true}, 10000, OWNER());
  check('b. the owner activates the vault item', byOwner.status === 200 && byOwner.body.item.status === 'active', byOwner);
  const audit = fs.readFileSync(path.join(amux.home, 'logs', 'vault-audit.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
  const activation = audit.filter(r => r.event === 'item_updated' && r.item === item && r.change?.activate).pop();
  check('b. the vault audit names the verified owner, not "owner" by default', activation?.by === 'owner-token', activation);
  const anonRow = audit.filter(r => r.event === 'item_updated' && r.item === item && r.change?.rules).pop();
  check('d. the headerless change is recorded as unproven', anonRow?.by === 'unproven (no worker header)', anonRow);

  // b/c. A grant the worker asked for.
  const g1 = grant('e2e-worker');
  const self = await amux.req('POST', `/api/grants/${g1}/approve`, undefined, 10000, WORKER);
  check('c. a worker cannot approve its own grant', self.status === 403, self);
  const approved = await amux.req('POST', `/api/grants/${g1}/approve`, undefined, 10000, OWNER());
  check('b. the owner approves it, verified', approved.status === 200 && approved.body.approved_by === 'owner-token' && approved.body.approver_verified === true, approved);

  // e. Enforce.
  await amux.down();
  amux.env.AMUX_ANONYMOUS_IS_OWNER = 'enforce';
  await amux.up();
  check('restarted in enforce mode', (await counters()).mode === 'enforce');
  const r1 = await amux.req('PATCH', `/api/vault/${item}`, {rules: {max_usd_without_approval: 9}}, 10000, {});
  check('e. a headerless vault change is refused', r1.status === 403 && r1.body.code === 'anonymous_refused', r1);
  const g2 = grant('e2e-worker');
  const r2 = await amux.req('POST', `/api/grants/${g2}/approve`, undefined, 10000, {});
  check('e. a headerless grant approval is refused', r2.status === 403 && r2.body.code === 'anonymous_refused', r2);
  check('e. the refused grant is still pending', fs.existsSync(path.join(amux.home, 'grants', g2 + '.json')));
  const r3 = await vaultItem({}, 'e2e-anon-login');
  check('e. a headerless vault item is held for the owner, not live', r3.status === 201 && r3.body.item.status === 'pending_owner_activation', r3);
  const r4 = await amux.req('POST', `/api/grants/${g2}/approve`, undefined, 10000, OWNER());
  check('e. the owner still approves under enforce', r4.status === 200 && r4.body.approver_verified === true, r4);
  const beforeCli = (await counters()).requests;
  const out = ownerCli('board', 'add', 'anonymous e2e: added from the owner terminal');
  check("e. the owner's CLI mutating call succeeded", /[A-Z]+-\d+/.test(out), out);
  const afterCli = (await counters()).requests;
  check("e. the owner's CLI proved itself (counted proven, nothing refused)",
    afterCli.proven > beforeCli.proven && afterCli.refused === beforeCli.refused, {beforeCli, afterCli});
  await waitFor('refused log line', () => logLines('anonymous_refused').length > 0, 10000);

  // f. Off.
  await amux.down();
  amux.env.AMUX_ANONYMOUS_IS_OWNER = 'off';
  await amux.up();
  const r5 = await amux.req('PATCH', `/api/vault/${item}`, {rules: {max_usd_without_approval: 3}}, 10000, {});
  check('f. AMUX_ANONYMOUS_IS_OWNER=off checks nothing', r5.status === 200 && (await counters()).mode === 'off', r5);
  ok = true;
} finally {
  const receipt = {scenario: 'anonymous-not-owner', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
