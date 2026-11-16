#!/usr/bin/env node
// END-TO-END (governance PR 19): auditor role, audit export, legal hold,
// a person's export and erasure, and the owner's kill switch, against a
// private amux (own home, port and tmux socket). Checks:
//   a. activity from the owner, a worker with its token and a member lands in
//      the ledger under each proven actor;
//   b. an auditor (a member with role auditor) reads the ledger, the export,
//      the at-rest report and the counters, and is refused every write and
//      every non-audit read; the role is set by the owner only;
//   c. the export: CSV with the chain verdict in its headers and a
//      chain_verified column, a worker filter, OCSF JSON lines (class 6003);
//      a worker cannot export; the export is itself a ledger row;
//   d. a legal hold on the member blocks her erasure (409) and the database
//      refuses to delete her rows; lifting it is recorded;
//   e. erasure: a dry run changes nothing; the erase replaces her name with
//      erased:<id> in every row, keeps every hash, the chain still verifies
//      (with the redacted count), and her person export is empty;
//   f. the kill switch: the auditor cannot engage it; the owner does; a
//      worker's publish, email send and connector mint are refused, its other
//      work is not; the git pre-push hook refuses the worker's push from the
//      same file; the counters say so; lifting it lets the worker through;
//   g. retention: with a one-second window and a hold on the worker, the
//      prune runs, drops the unheld prefix and stops at the worker's first
//      row; the chain verifies from the moved anchor;
//   h. every power used above is a `governance` row in the ledger.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/audit-governance.mjs
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import https from 'node:https';
import crypto from 'node:crypto';
import {execFileSync, spawnSync} from 'node:child_process';
import {startAmux} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', AMUX_VAULT_KEYSTORE: 'file',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail).slice(0, 2000));
  console.log('ok -', name);
};
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-I', '-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const dbTry = (sql, args = []) => { try { db(sql, args); return null; } catch (e) { return String(e.stderr || e.message); } };
const formPost = (p, form) => new Promise((resolve, reject) => {
  const data = Buffer.from(new URLSearchParams(form).toString());
  const r = https.request(amux.base + p, {method: 'POST', rejectUnauthorized: false, timeout: 15000,
    headers: {'Content-Type': 'application/x-www-form-urlencoded', 'Content-Length': data.length}}, res => {
    res.resume(); res.on('end', () => resolve({status: res.statusCode, headers: res.headers}));
  });
  r.on('error', reject); r.write(data); r.end();
});
const raw = (p, headers) => new Promise((resolve, reject) => {
  const r = https.request(amux.base + p, {method: 'GET', rejectUnauthorized: false, timeout: 30000, headers}, res => {
    let buf = ''; res.on('data', c => buf += c); res.on('end', () => resolve({status: res.statusCode, headers: res.headers, text: buf}));
  });
  r.on('error', reject); r.end();
});
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };

// A worker with its own folder and token.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-gov19-'));
const wdir = path.join(root, 'customers', 'wexus');
fs.mkdirSync(wdir, {recursive: true});
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
fs.writeFileSync(path.join(amux.home, 'sessions', 'wexus.env'), `CC_DIR=${wdir}\nCC_TAGS=customers\nCC_ARCHIVED=0\n`);
const wtoken = crypto.randomBytes(24).toString('hex');
fs.writeFileSync(path.join(amux.home, 'worker-tokens', 'wexus.sha256'),
  crypto.createHash('sha256').update(wtoken).digest('hex'), {mode: 0o600});
const WEXUS = {'X-Amux-Session': 'wexus', 'X-Amux-Worker-Token': wtoken};
// A fresh fake grant, so a mint never leaves the box.
const X = 'x@e2e.invalid';
fs.mkdirSync(path.join(amux.home, 'connectors', 'google'), {recursive: true});
fs.writeFileSync(path.join(amux.home, 'connectors', 'google', X + '.json'), JSON.stringify({
  token: 'FAKE-BEARER-X', scopes: 'https://www.googleapis.com/auth/gmail.modify', expires_at: Date.now() / 1000 + 3600,
}), {mode: 0o600});
const ANN = 'ann@e2e.invalid', AUD = 'aud@e2e.invalid';

const member = async email => {
  const inv = await amux.req('POST', '/api/org/invites', {email, scope_level: 'global', grant_host_access: true}, 10000, OWNER());
  const tok = inv.body.token || String(inv.body.url || '').split('/invite/')[1];
  const acc = await formPost(`/invite/${tok}`, {email, name: email.split('@')[0]});
  return (acc.headers['set-cookie'] || []).map(c => c.split(';')[0]).find(c => c.startsWith('amux_member='));
};
const ledgerRows = (where, args = []) => db(`select seq, event_id, kind, actor, claimed, resource, action, hash, redacted_at from _amux_ledger where ${where} order by seq`, args);
const verify = async () => (await amux.req('GET', '/api/ledger/verify', undefined, 30000, OWNER())).body;

let ok = false;
try {
  await restart({});
  // a. Activity under three proven actors.
  const annCookie = await member(ANN);
  const audCookie = await member(AUD);
  check('a. two members accept and hold cookies', !!annCookie && !!audCookie, {annCookie, audCookie});
  const ANNH = {Cookie: annCookie}, AUDH = {Cookie: audCookie};
  for (const [h, t] of [[OWNER(), 'owner card'], [WEXUS, 'wexus card'], [ANNH, 'ann card'], [WEXUS, 'wexus card 2'], [ANNH, 'ann card 2']]) {
    const r = await amux.req('POST', '/api/board', {title: t, status: 'todo', session: 'wexus'}, 10000, h);
    // Boards are off by default, so a worker's card may be refused; the command
    // is recorded under its proven actor either way.
    check(`a. ${t} is answered`, r.status > 0 && r.status < 500, r);
  }
  await sleep(500);
  check('a. the ledger names the worker and the member as proven actors',
    ledgerRows("actor='worker:wexus'").length >= 2 && ledgerRows('actor=?', ['member:' + ANN]).length >= 2,
    db('select actor, count(*) n from _amux_ledger group by actor'));

  // b. The auditor.
  const members = (await amux.req('GET', '/api/org/members', undefined, 10000, OWNER())).body;
  const audId = (Array.isArray(members) ? members : members.members || []).find(m => m.email === AUD)?.id;
  let r = await amux.req('PATCH', `/api/org/members/${audId}`, {role: 'auditor'}, 10000, ANNH);
  check('b. a member cannot make anyone an auditor', r.status === 403, r);
  r = await amux.req('PATCH', `/api/org/members/${audId}`, {role: 'auditor'}, 10000, OWNER());
  check('b. the owner makes aud@ an auditor', r.status === 200 && r.body.role === 'auditor', r);
  for (const p of ['/api/ledger?limit=5', '/api/ledger/verify', '/api/audit/export?format=json', '/api/security/at-rest',
                   '/api/contract/counters', '/api/audit/holds', '/api/approvals/cards', '/api/audit/kill-switch']) {
    r = await amux.req('GET', p, undefined, 30000, AUDH);
    check(`b. the auditor reads ${p}`, r.status === 200, {status: r.status, body: r.body});
  }
  for (const [m, p, b] of [['POST', '/api/board', {title: 'auditor card'}], ['GET', '/api/sessions'], ['GET', '/api/fs/list?path=/'],
                           ['POST', '/api/audit/holds', {all: true}], ['POST', '/api/audit/kill-switch', {engaged: true}],
                           ['POST', '/api/audit/person/erase', {subject: ANN}], ['POST', '/api/sessions/wexus/send', {text: 'hi'}]]) {
    r = await amux.req(m, p, b, 10000, AUDH);
    check(`b. the auditor is refused ${m} ${p}`, r.status === 403, {status: r.status, body: r.body});
  }

  // c. Export.
  r = await raw('/api/audit/export?format=csv', OWNER());
  const lines = r.text.trim().split('\n');
  check('c. CSV export: 200, header row, chain verdict in the headers',
    r.status === 200 && lines[0].startsWith('seq,event_id,ts,at,kind,actor') && r.headers['x-amux-ledger-chain-ok'] === 'true'
      && Number(r.headers['x-amux-ledger-rows']) === lines.length - 1 && lines.length > 5, {h: r.headers, head: lines.slice(0, 2)});
  check('c. every exported row is chain-verified', lines.slice(1).every(l => l.endsWith(',true')), lines.slice(-3));
  check('c. the CSV names the member', r.text.includes('member:' + ANN), lines.slice(0, 4));
  r = await raw('/api/audit/export?format=ocsf&worker=wexus', OWNER());
  const ev = r.text.trim().split('\n').map(l => JSON.parse(l));
  check('c. OCSF JSON lines, API Activity, filtered to the worker',
    ev.length >= 2 && ev.every(e => e.class_uid === 6003 && e.unmapped.amux.chain_verified === true)
      && ev.every(e => JSON.stringify(e).includes('wexus')), ev.slice(0, 2));
  r = await amux.req('GET', '/api/audit/export?format=json', undefined, 10000, WEXUS);
  check('c. a worker cannot export the record', r.status === 403, r);
  check('c. each export is itself a ledger row', ledgerRows("kind='governance' and action='audit.export'").length >= 3,
    ledgerRows("kind='governance'"));

  // d. Legal hold on the member.
  r = await amux.req('POST', '/api/audit/holds', {name: 'matter 7', actor: 'member:' + ANN}, 10000, OWNER());
  const hold = r.body.id;
  check('d. the owner places a hold, which counts her rows', r.status === 200 && r.body.held_rows >= 2, r);
  r = await amux.req('POST', '/api/audit/holds', {all: true}, 10000, WEXUS);
  check('d. a worker cannot place a hold', r.status === 403, r);
  r = await amux.req('POST', '/api/audit/person/erase', {subject: ANN}, 10000, OWNER());
  check('d. her erasure is refused under the hold, naming it', r.status === 409 && r.body.code === 'legal_hold' && r.body.holds.includes(hold), r);
  const delErr = dbTry('delete from _amux_ledger where actor=?', ['member:' + ANN]);
  check('d. the database refuses to delete a held row', delErr && delErr.includes('legal hold'), delErr);
  r = await amux.req('POST', `/api/audit/holds/${hold}/lift`, undefined, 10000, OWNER());
  check('d. the owner lifts the hold', r.status === 200, r);

  // e. Erasure.
  const before = ledgerRows("actor=? or resource like ?", ['member:' + ANN, '%' + ANN + '%']);
  r = await amux.req('POST', '/api/audit/person/erase', {subject: ANN, dry_run: true}, 10000, OWNER());
  check('e. a dry run counts and changes nothing', r.status === 200 && r.body.dry_run === true && r.body.ledger_rows >= 2
    && ledgerRows('actor=?', ['member:' + ANN]).length === before.filter(x => x.actor === 'member:' + ANN).length, r);
  r = await amux.req('POST', '/api/audit/person/erase', {subject: 'member:' + ANN}, 10000, ANNH);
  check('e. she cannot erase herself (the owner decides)', r.status === 403, r);
  r = await amux.req('POST', '/api/audit/person/erase', {subject: ANN}, 10000, OWNER());
  check('e. the owner erases her', r.status === 200 && /^era_/.test(r.body.erasure_id) && r.body.ledger_rows >= 2, r);
  const token = r.body.token;
  const still = db("select count(*) n from _amux_ledger where instr(lower(actor),?)>0 or instr(lower(coalesce(resource,'')),?)>0 or instr(lower(coalesce(claimed,'')),?)>0", [ANN, ANN, ANN])[0].n;
  check('e. no ledger row names her any more', still === 0, still);
  const after = db(`select seq, hash, actor from _amux_ledger where seq in (${before.map(b => b.seq).join(',')}) order by seq`);
  check('e. every redacted row kept its hash and carries the erasure token',
    after.length === before.length && after.every((a, i) => a.hash === before[i].hash) && after.some(a => a.actor === token), {before, after});
  const v = await verify();
  check('e. the chain verifies with the redacted rows counted', v.chain_ok === true && v.redacted >= 2, v);
  r = await amux.req('GET', `/api/audit/person?subject=${encodeURIComponent(ANN)}`, undefined, 10000, AUDH);
  check('e. her person export (read by the auditor) holds no ledger row and names what it cannot reach',
    r.status === 200 && r.body.ledger.length === 0 && r.body.not_included.length >= 3, r.body);
  const tamper = dbTry("update _amux_ledger set decision='deny' where actor=?", [token]);
  check('e. a redacted row still cannot be changed', tamper && tamper.includes('append-only'), tamper);

  // f. Kill switch.
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: true, reason: 'e2e incident'}, 10000, WEXUS);
  check('f. a worker cannot engage the kill switch', r.status === 403, r);
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: true, reason: 'e2e incident'}, 10000, OWNER());
  check('f. the owner engages it', r.status === 200 && r.body.engaged === true, r);
  const outbound = [
    ['POST', '/api/page-links', {port: 5555, title: 'x'}, 'publish'],
    ['POST', '/api/email/send', {to: 'buyer@customer.example', subject: 's', body: 'b'}, 'email_send'],
    ['POST', `/api/connectors/google-gmail/token?account=${encodeURIComponent(X)}`, undefined, 'connector_mint'],
  ];
  for (const [m, p, b, kind] of outbound) {
    r = await amux.req(m, p, b, 10000, WEXUS);
    check(`f. the worker's ${kind} is stopped`, r.status === 403 && r.body.code === 'kill_switch_engaged' && r.body.kind === kind, r);
  }
  r = await amux.req('POST', '/api/board', {title: 'still working', status: 'todo', session: 'wexus'}, 10000, WEXUS);
  check('f. the worker\'s other work is not stopped by the switch', r.status < 500 && r.body.code !== 'kill_switch_engaged', r);
  r = await amux.req('POST', `/api/connectors/google-gmail/token?account=${encodeURIComponent(X)}`, undefined, 10000, OWNER());
  check('f. the owner is not stopped', r.status === 200 && r.body.access_token === 'FAKE-BEARER-X', r);
  const repo = path.join(root, 'repo');
  spawnSync('git', ['init', '-q', repo]);
  const hook = spawnSync('python3', [path.join(process.cwd(), 'scripts', 'git-hooks', 'pre-push'), 'origin', 'file:///dev/null'],
    {cwd: repo, input: '', encoding: 'utf8', env: {...process.env, AMUX_HOME: amux.home, AMUX_SESSION: 'wexus', AMUX_URL: amux.base}});
  check('f. the pre-push hook refuses the worker\'s push from the same switch',
    hook.status === 1 && hook.stderr.includes('kill_switch_refused') && hook.stderr.includes('e2e incident'), hook.stderr);
  const gc = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.governance || {};
  check('f. the counters show the switch engaged and the refusals', gc.kill_switch?.engaged === true && gc.kill_switch.refused >= 3 && gc.measured === true, gc);
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: false}, 10000, OWNER());
  check('f. the owner lifts it', r.status === 200 && r.body.engaged === false, r);
  r = await amux.req('POST', `/api/connectors/google-gmail/token?account=${encodeURIComponent(X)}`, undefined, 10000, WEXUS);
  check('f. after the lift the worker is no longer stopped by the switch', r.body.code !== 'kill_switch_engaged', r);

  // g. Retention with a hold on the worker.
  r = await amux.req('POST', '/api/audit/holds', {name: 'wexus matter', worker: 'wexus'}, 10000, OWNER());
  check('g. a hold on the worker', r.status === 200 && r.body.held_rows >= 2, r);
  const firstWexus = ledgerRows("actor='worker:wexus'")[0].seq;
  check('g. there are unheld rows before the worker\'s first row', ledgerRows('seq < ?', [firstWexus]).length > 0);
  await restart({AMUX_LEDGER_RETAIN_DAYS: '0.00002'});
  await sleep(2500);
  // The retention sweep runs once per 1000 logged requests.
  for (let i = 0; i < 1100; i += 50) {
    await Promise.all(Array.from({length: 50}, () => amux.req('GET', '/api/identity', undefined, 10000, OWNER())));
  }
  let anchor = 0;
  for (let i = 0; i < 40 && anchor === 0; i++) {
    await sleep(500);
    anchor = db('select upto_seq from _amux_ledger_anchor where id=1')[0].upto_seq;
  }
  check('g. the prune ran and moved the anchor', anchor > 0, anchor);
  check('g. it stopped at the worker\'s first held row', anchor < firstWexus && ledgerRows("actor='worker:wexus'").length >= 2,
    {anchor, firstWexus});
  const v2 = await verify();
  check('g. the chain verifies from the moved anchor', v2.chain_ok === true && v2.anchor.upto_seq === anchor, v2);
  const lc = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.ledger || {};
  check('g. the ledger counts rows a hold kept past retention', lc.held_back_by_legal_hold > 0, lc);

  // h. Every power is in the record.
  const gov = ledgerRows("kind='governance'").map(x => x.action);
  for (const a of ['audit.export', 'legal_hold.place', 'legal_hold.lift', 'person.erase', 'kill_switch.engage', 'kill_switch.lift']) {
    check(`h. the ledger records ${a}`, gov.includes(a), gov);
  }
  ok = true;
} finally {
  const receipt = {scenario: 'audit-governance', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
  fs.rmSync(root, {recursive: true, force: true});
}
process.exit(ok ? 0 : 1);
