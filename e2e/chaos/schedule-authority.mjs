#!/usr/bin/env node
// END-TO-END (governance PR 13): a schedule keeps its proven creator, every
// cron firing re-checks that creator's authority, and offboarding a member
// pauses or reassigns what they own in one action.
//
// A private amux with worker sa-w (group customers) and a member M scoped to it.
// Checks:
//   a. M creates a schedule with their member cookie: it records
//      created_by=member:<M>, verified;
//   b. M's scope is moved off sa-w (in the database: member scope changes go
//      through teams, which this scenario does not need);
//   c. report (default): the next cron firing still runs, is counted
//      would_skip, and the ledger records would_deny;
//   d. enforce: the next firing is skipped, the schedule is paused (enabled=0,
//      disabled_reason=authority_lapsed, not deleted), ONE Needs-you card asks
//      the owner, and a second lapse files no second card;
//   e. the owner re-owns it: created_by=owner, verified, re-enabled, ask cleared;
//   f. a worker cannot offboard anyone;
//   g. offboard M, dry run: lists M's sign-in, a second schedule, a standing
//      approval M granted, an app M approved and a worker M sponsors, and
//      changes nothing (M's cookie still verifies);
//   h. offboard M: the schedule is paused (not deleted), the approval revoked,
//      the app back to pending, the sponsor the owner, one ledger event per
//      item, and M's cookie no longer verifies;
//   i. AMUX_SCHEDULE_AUTHORITY=off reports mode off.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/schedule-authority.mjs
import fs from 'node:fs';
import path from 'node:path';
import https from 'node:https';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_RS_SCHEDULER: '0', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0',
  AMUX_GHOST_RESCUE_SECS: '0', AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-I', '-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const formPost = (p, form) => new Promise((resolve, reject) => {
  const data = Buffer.from(new URLSearchParams(form).toString());
  const r = https.request(amux.base + p, {method: 'POST', rejectUnauthorized: false, timeout: 15000,
    headers: {'Content-Type': 'application/x-www-form-urlencoded', 'Content-Length': data.length}}, res => {
    res.resume(); res.on('end', () => resolve({status: res.statusCode, headers: res.headers}));
  });
  r.on('error', reject); r.write(data); r.end();
});
const counters = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.schedule_authority || {};
const ledger = async () => (await amux.req('GET', '/api/ledger?limit=2000', undefined, 10000, OWNER())).body.events || [];
const cards = async () => {
  const r = await amux.req('GET', '/api/board', undefined, 10000, OWNER());
  const items = Array.isArray(r.body) ? r.body : (r.body.items || r.body.issues || []);
  return items.filter(c => c.source === 'schedule-authority' || c.creator === 'schedule-authority');
};
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };
const row = id => db('select enabled, disabled_reason, deleted, created_by, created_by_verified, run_count from schedules where id=?', [id])[0];
const M = 'm@e2e.invalid';

let ok = false;
try {
  const dir = path.join(amux.root, 'sa-w'); fs.mkdirSync(dir);
  const w = await amux.req('POST', '/api/sessions', {name: 'sa-w', dir, start: false}, 15000, OWNER());
  check('setup: worker sa-w', w.status === 201, w);
  fs.appendFileSync(path.join(amux.home, 'sessions', 'sa-w.env'), '\nCC_TAGS=customers\n');
  const inv = await amux.req('POST', '/api/org/invites', {email: M, scope_level: 'worker', scope_name: 'sa-w'}, 10000, OWNER());
  const tok = inv.body.token || String(inv.body.url || '').split('/invite/')[1];
  const acc = await formPost(`/invite/${tok}`, {email: M, name: 'M'});
  const cookie = (acc.headers['set-cookie'] || []).map(c => c.split(';')[0]).find(c => c.startsWith('amux_member='));
  check('setup: member M scoped to sa-w holds a cookie', !!cookie, acc);
  const AS_M = {Cookie: cookie};
  const identity = async () => (await amux.req('GET', '/api/identity', undefined, 10000, AS_M)).body;
  check('setup: the cookie verifies as M', (await identity()).is_local_member === true, await identity());

  // a.
  const s1 = await amux.req('POST', '/api/schedules', {title: 'sa e2e', session: 'sa-w', kind: 'shell',
    command: 'true', schedule_expr: 'daily at 3am', enabled: 0}, 10000, AS_M);
  const S1 = s1.body.id || s1.body.schedule?.id;
  check('a. M creates a schedule', !!S1, s1);
  let r1 = row(S1);
  check('a. ...recorded with M as its proven creator', r1.created_by === `member:${M}` && r1.created_by_verified === 1, r1);

  // b.
  db("update org_members set scope_name='elsewhere' where email=?", [M]);
  check('b. M no longer has scope over sa-w', db('select scope_name from org_members where email=?', [M])[0].scope_name === 'elsewhere');

  // c. report
  await amux.down();
  db("update schedules set enabled=1, next_run='2020-01-01T00:00' where id=?", [S1]);
  amux.env.AMUX_RS_SCHEDULER = '1';
  await amux.up();
  await waitFor('report-mode firing', async () => ((await counters()).counts || {}).would_skip >= 1, 60000);
  let c = await counters();
  check('c. report mode, measured', c.mode === 'report' && c.measured === true, c);
  await waitFor('the firing ran', () => (row(S1).run_count || 0) >= 1 || db('select count(*) n from schedule_runs where schedule_id=?', [S1])[0].n >= 1, 30000);
  check('c. the schedule still fired and stays enabled', row(S1).enabled === 1 || row(S1).disabled_reason !== 'authority_lapsed', row(S1));
  const wouldDeny = (await ledger()).find(e => e.kind === 'schedule_authority' && e.decision === 'would_deny' && (e.resource || '').includes(S1));
  check('c. the ledger records would_deny on behalf of M', wouldDeny && wouldDeny.on_behalf_of === `member:${M}`, wouldDeny);
  check('c. report files no card', (await cards()).length === 0, await cards());

  // d. enforce
  await amux.down();
  db("update schedules set enabled=1, disabled_reason=NULL, next_run='2020-01-01T00:00' where id=?", [S1]);
  Object.assign(amux.env, {AMUX_SCHEDULE_AUTHORITY: 'enforce'});
  await amux.up();
  await waitFor('enforce pause', () => row(S1).disabled_reason === 'authority_lapsed', 60000);
  r1 = row(S1);
  check('d. enforce pauses the schedule, never deletes it', r1.enabled === 0 && r1.deleted === null, r1);
  c = await counters();
  check('d. counted skipped', (c.counts || {}).skipped >= 1 && c.mode === 'enforce', c);
  await waitFor('one card', async () => (await cards()).length === 1, 15000);
  const card = (await cards())[0];
  check('d. one Needs-you access card asks the owner', card.status === 'needsyou' && card.ask_type === 'access', card);
  await amux.down();
  db("update schedules set enabled=1, disabled_reason=NULL, next_run='2020-01-01T00:00' where id=?", [S1]);
  await amux.up();
  await waitFor('second pause', () => row(S1).disabled_reason === 'authority_lapsed', 60000);
  await new Promise(r => setTimeout(r, 2000));
  check('d. a second lapse files no second card', (await cards()).length === 1 && ((await counters()).counts || {}).card_deduped >= 1, await counters());

  // e.
  const ro = await amux.req('POST', `/api/schedules/${S1}/reown`, undefined, 10000, OWNER());
  check('e. the owner re-owns it', ro.status === 200 && ro.body.created_by === 'owner', ro);
  r1 = row(S1);
  check('e. ...owner, verified, re-enabled', r1.created_by === 'owner' && r1.created_by_verified === 1 && r1.enabled === 1 && r1.disabled_reason === null, r1);
  const asks = JSON.parse(fs.readFileSync(path.join(amux.home, 'schedule-authority-asks.json'), 'utf8'));
  check('e. ...and its ask is cleared', !(S1 in asks), asks);

  // Stop the scheduler for the offboarding half.
  await restart({AMUX_RS_SCHEDULER: '0', AMUX_SCHEDULE_AUTHORITY: 'report'});

  // f.
  const wtok = crypto.randomBytes(24).toString('hex');
  fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
  fs.writeFileSync(path.join(amux.home, 'worker-tokens', 'sa-w.sha256'), crypto.createHash('sha256').update(wtok).digest('hex'));
  const fw = await amux.req('POST', `/api/org/members/${encodeURIComponent(M)}/offboard?dry_run=1`, undefined, 10000,
    {...OWNER(), 'X-Amux-Session': 'sa-w', 'X-Amux-Worker-Token': wtok});
  check('f. a worker cannot offboard anyone', fw.status === 403, fw);

  // g. what M owns
  db("update org_members set scope_name='sa-w' where email=?", [M]);
  const s2 = await amux.req('POST', '/api/schedules', {title: 'sa e2e 2', session: 'sa-w', kind: 'shell',
    command: 'true', schedule_expr: 'daily at 4am'}, 10000, AS_M);
  const S2 = s2.body.id || s2.body.schedule?.id;
  check('g. M creates a second schedule', !!S2 && row(S2).created_by === `member:${M}`, s2);
  db("insert into standing_approvals(title,allowed,category,granted_by,granted_at) values('sa e2e','x','budget',?,0)", [`member:${M}`]);
  const linksFile = path.join(amux.home, 'public-links.json');
  const links = fs.existsSync(linksFile) ? JSON.parse(fs.readFileSync(linksFile, 'utf8')) : {links: []};
  links.links = links.links || [];
  links.links.push({id: 'saapp' + crypto.randomBytes(4).toString('hex'), kind: 'file', path: dir, title: 'sa e2e app',
    created_by: 'sa-w', created_at: 0, approved_by: `member:${M}`});
  fs.writeFileSync(linksFile, JSON.stringify(links));
  const APP = links.links.at(-1).id;
  const sp = await amux.req('PUT', '/api/workers/sa-w/sponsor', {sponsor: `member:${M}`}, 10000, OWNER());
  check('g. the owner makes M the sponsor of sa-w', sp.status === 200, sp);
  const dry = await amux.req('POST', `/api/org/members/${encodeURIComponent(M)}/offboard?dry_run=1`, undefined, 10000, OWNER());
  const kinds = (dry.body.items || []).map(i => i.kind);
  check('g. dry run lists sign-in, schedule, approval, app and sponsorship',
    dry.status === 200 && dry.body.dry_run === true && dry.body.measured === true
      && ['session', 'schedule', 'standing_approval', 'published_app', 'worker_sponsor'].every(k => kinds.includes(k)), dry.body);
  check('g. the dry run shows no tokens', !JSON.stringify(dry.body).includes(tok), 'token leaked');
  check('g. ...and changes nothing', row(S2).enabled === 1 && (await identity()).is_local_member === true, row(S2));

  // h. offboard
  const before = (await ledger()).filter(e => e.kind === 'offboard').length;
  const off = await amux.req('POST', `/api/org/members/${encodeURIComponent(M)}/offboard`, undefined, 10000, OWNER());
  check('h. offboard succeeds', off.status === 200 && off.body.dry_run === false, off);
  const r2 = row(S2);
  check('h. M\'s schedule is paused, not deleted', r2.enabled === 0 && r2.disabled_reason === 'creator_offboarded' && r2.deleted === null, r2);
  check('h. the standing approval M granted is revoked', db('select revoked from standing_approvals where granted_by=?', [`member:${M}`])[0].revoked === 1);
  const app = JSON.parse(fs.readFileSync(linksFile, 'utf8')).links.find(l => l.id === APP);
  check('h. the app M approved is back to pending', app && (app.approved_by || '') === '', app);
  check('h. sa-w\'s sponsor is the owner', db('select sponsor from worker_sponsors where worker=?', ['sa-w'])[0].sponsor === 'owner');
  const offEvents = (await ledger()).filter(e => e.kind === 'offboard').length - before;
  check('h. one ledger event per item', offEvents === off.body.items.length && offEvents >= 5, {offEvents, items: off.body.items.length});
  check('h. M\'s cookie no longer verifies', (await identity()).is_local_member !== true, await identity());
  check('h. M is stamped offboarded, row kept', db('select offboarded_at from org_members where email=?', [M])[0].offboarded_at > 0);

  // i.
  await restart({AMUX_SCHEDULE_AUTHORITY: 'off'});
  check('i. AMUX_SCHEDULE_AUTHORITY=off reports off', (await counters()).mode === 'off', await counters());

  ok = true;
} finally {
  await amux.down();
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length}));
  if (!ok) process.exitCode = 1;
}
