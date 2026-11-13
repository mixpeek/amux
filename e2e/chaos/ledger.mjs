#!/usr/bin/env node
// END-TO-END (governance PR 7): the ledger is the append-only, hash-chained
// record of who did what, under what authority.
//
// A private amux (own home, port and tmux socket) with worker identity in
// enforce, so a forged worker claim is refused. Checks:
//   a. the owner's action lands with actor `owner`, authority
//      `owner_credential`;
//   b. a worker's action with its own token lands as `worker:<name>`,
//      `worker_token`;
//   c. a published app's allowed call with its own token lands as
//      `app:<id>`, `app_token`, on behalf of its publisher;
//   d. a forged worker claim is refused and recorded: actor `anonymous`,
//      the name only under `claimed`, decision `refused`;
//   e. a policy receipt (a connector token mint) lands in the ledger too;
//      telemetry (typing) does not;
//   f. GET /api/ledger/verify: chain ok, sealed, measured; `/api/audit` shows
//      the worker's ledger rows;
//   g. deleting the worker leaves its ledger rows;
//   h. a row changed directly in SQLite (triggers dropped, as an attacker
//      with the file would) is named by verify; an UPDATE with the triggers
//      in place is refused.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/ledger.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', AMUX_WORKER_IDENTITY: 'enforce',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, ['auth', 'token'].join('_')), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const PERSON = () => ({...OWNER(), Cookie: '__Host-amux_owner=' + crypto.createHash('sha256').update('amux-owner-session:' + ownerToken()).digest('hex')});
const UI = () => ({...OWNER(), 'X-Amux-UI-Token': crypto.createHash('sha256').update('amux-ui-guard:' + ownerToken()).digest('hex').slice(0, 40)});
const ledger = async q => (await amux.req('GET', '/api/ledger?limit=1000&' + (q || ''), undefined, 10000, OWNER())).body;
const find = async (pred, q) => ((await ledger(q)).events || []).filter(pred);
const dbPath = path.join(amux.home, 'amux.db');
const sql = stmts => execFileSync('python3', ['-I', '-c', `
import sqlite3, sys
c = sqlite3.connect(sys.argv[1], timeout=30)
out = []
for s in sys.argv[2:]:
    try:
        cur = c.execute(s); out.append(repr(cur.fetchall()))
    except Exception as e:
        out.append('ERR ' + str(e))
c.commit(); print('\\n'.join(out))
`, dbPath, ...stmts], {encoding: 'utf8'}).trim().split('\n');

// A worker with a minted identity token on disk (the launch path mints it).
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
fs.writeFileSync(path.join(amux.home, 'sessions', 'w-ledger.env'), 'CC_TAGS=""\nCC_ARCHIVED=0\nCC_DIR="/tmp"\nCC_AUTO_PICKUP="1"\n');
const workerToken = crypto.randomBytes(24).toString('hex');
fs.writeFileSync(path.join(amux.home, 'worker-tokens', 'w-ledger.sha256'),
  crypto.createHash('sha256').update(workerToken).digest('hex'), {mode: 0o600});
const WORKER = {'X-Amux-Session': 'w-ledger', 'X-Amux-Worker-Token': workerToken};

let ok = false;
try {
  // a. Owner.
  let r = await amux.req('POST', '/api/board', {title: 'ledger e2e: owner', status: 'todo'}, 10000, OWNER());
  check('a. the owner files a card', r.status >= 200 && r.status < 300, r);
  let rows = await find(e => e.action === 'POST /api/board' && e.actor === 'owner');
  check('a. ...recorded as owner on the owner credential', rows.length >= 1 && rows[0].authority === 'owner_credential' && rows[0].decision === 'allowed', rows);

  // b. Worker with its own token.
  r = await amux.req('POST', '/api/board', {title: 'ledger e2e: worker', status: 'todo'}, 10000, WORKER);
  check('b. the worker files a card with its token', r.status >= 200 && r.status < 300, r);
  rows = await find(e => e.action === 'POST /api/board' && e.actor === 'worker:w-ledger');
  check('b. ...recorded as the worker on its token', rows.length >= 1 && rows[0].authority === 'worker_token' && !rows[0].claimed, rows);

  // c. A published app's allowed call.
  const pub = await amux.req('POST', '/api/page-links', {kind: 'app', port: 59991, title: 'ledger app', api: ['POST /api/board']}, 10000, PERSON());
  check('c. the owner publishes an app allowed POST /api/board', pub.status === 201 && !!pub.body.app_token, pub);
  r = await amux.req('POST', '/api/board', {title: 'ledger e2e: app', status: 'todo'}, 10000, {'X-Amux-App-Token': pub.body.app_token});
  check('c. the app files a card with its token', r.status >= 200 && r.status < 300, r);
  rows = await find(e => e.action === 'POST /api/board' && e.actor === `app:${pub.body.id}`);
  check('c. ...recorded as the app on its token, on behalf of its publisher',
    rows.length >= 1 && rows[0].authority === 'app_token' && !!rows[0].on_behalf_of, rows);

  // d. A forged worker claim.
  r = await amux.req('POST', '/api/board', {title: 'ledger e2e: forged', status: 'todo'}, 10000,
    {...OWNER(), Authorization: '', 'X-Amux-Session': 'w-ledger', 'X-Amux-Worker-Token': 'not-the-token'});
  check('d. a forged worker claim is refused', r.status === 403 || r.status === 401, r);
  rows = await find(e => e.action === 'POST /api/board' && e.claimed === 'worker:w-ledger');
  check('d. ...recorded as anonymous, the name only claimed, refused',
    rows.length >= 1 && rows[0].actor === 'anonymous' && ['refused', 'failed'].includes(rows[0].decision), rows);

  // e. A policy receipt, and telemetry left out.
  r = await amux.req('POST', '/api/connectors/google-gmail/token', {}, 10000, {'X-Amux-Session': 'w-ledger', 'X-Amux-Worker-Token': workerToken});
  rows = await find(e => e.kind === 'policy' && e.action === 'connector_token_mint');
  check('e. a connector token mint receipt lands in the ledger', rows.length >= 1 && rows[0].actor === 'worker:w-ledger', {mint: r.status, rows});
  await amux.req('POST', '/api/sessions/w-ledger/typing', {}, 10000, WORKER);
  rows = await find(e => String(e.action || '').endsWith('/typing'));
  check('e. telemetry (typing) is not recorded', rows.length === 0, rows);

  // f. Verify and audit.
  let v = (await amux.req('GET', '/api/ledger/verify', undefined, 10000, OWNER())).body;
  check('f. verify: chain ok, all sealed, measured', v.chain_ok === true && v.ok === true && v.unsealed === 0
    && v.sealed >= 5 && v.measured === true && v.n_considered === v.sealed, v);
  const audit = (await amux.req('GET', '/api/audit?worker=w-ledger&since_h=1', undefined, 10000, OWNER())).body;
  const src = (audit.sources || []).find(s => s.table === '_amux_ledger');
  check('f. /api/audit reads the ledger for the worker', src && src.error === null && src.rows_total >= 2
    && (audit.events || []).some(e => e.kind === 'ledger' && e.detail?.actor === 'worker:w-ledger'), audit);
  const c = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.ledger || {};
  check('f. the counters report the ledger, measured', c.enabled === true && c.measured === true && c.appended >= 5 && c.append_failed === 0, c);

  // g. Deleting the worker keeps its record.
  const before = (await find(e => e.actor === 'worker:w-ledger' || e.claimed === 'worker:w-ledger')).length;
  r = await amux.req('DELETE', '/api/sessions/w-ledger', undefined, 15000, UI());
  check('g. the worker is deleted', r.status >= 200 && r.status < 300, r);
  await waitFor('the worker gone', async () => !fs.existsSync(path.join(amux.home, 'sessions', 'w-ledger.env')), 10000);
  const after = (await find(e => e.actor === 'worker:w-ledger' || e.claimed === 'worker:w-ledger')).length;
  check('g. ...and its ledger rows remain', after >= before && before >= 3, {before, after});

  // h. Tamper.
  const target = (await find(e => e.actor === 'worker:w-ledger' && e.kind === 'interaction'))[0];
  const refused = sql([`UPDATE _amux_ledger SET actor='owner' WHERE event_id='${target.event_id}'`]);
  check('h. with the triggers in place an UPDATE is refused', /append-only/.test(refused[0]), refused);
  // The actor column has its own trigger since migration 0163 (erasure).
  const forced = sql(['DROP TRIGGER amux_ledger_no_update', 'DROP TRIGGER IF EXISTS amux_ledger_erase_only',
    `UPDATE _amux_ledger SET actor='owner' WHERE event_id='${target.event_id}'`]);
  check('h. an attacker with the file drops the trigger and rewrites the actor', !forced.some(l => l.startsWith('ERR')), forced);
  v = (await amux.req('GET', '/api/ledger/verify', undefined, 10000, OWNER())).body;
  check('h. verify names the changed row', v.chain_ok === false && v.first_bad_event === target.event_id
    && /changed/.test(v.first_bad?.reason || ''), v);
  ok = true;
} finally {
  const receipt = {scenario: 'ledger', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
