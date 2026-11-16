#!/usr/bin/env node
// END-TO-END (governance PR 21): the compliance views and the evidence pack,
// against a private amux (own home, port and tmux socket). Checks:
//   a. a worker with its token, in a git repo, acts with a traceparent; an agent
//      commit carries that trace in its Amux-Trace trailer;
//   b. the access review (owner): every section measured; the active member and
//      the active worker are not flagged; a member gone for 200 days, a SCIM
//      token never used, a standing approval with no expiry and a link pending
//      for 200 days each are, under their own rule; the worker carries its
//      effective capabilities;
//   c. who may read: an auditor reads both views; a plain member and a worker
//      are refused; a write to the views is refused;
//   d. the timeline for the trace: the worker's ledger rows and its commit,
//      oldest first, with the chain verified; by worker it finds the commit by
//      the agent author; no filter and a malformed trace are refused;
//   e. `amux compliance evidence`: every file answered 200, each sha256 in the
//      manifest matches the file, the chain verified, verdict complete, exit 0;
//      the CLI's access review and timeline print;
//   f. after a ledger row is rewritten behind the triggers, the evidence pack
//      says incomplete, ledger_chain_ok false, and exits 3; the timeline's chain
//      says the same;
//   g. the counters count the reads and the refusals.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/compliance.mjs
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
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail).slice(0, 2500));
  console.log('ok -', name);
};
const sleep = ms => new Promise(r => setTimeout(r, ms));
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-I', '-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const dbScript = sql => execFileSync('python3', ['-I', '-c',
  'import sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.executescript(sys.argv[2]); c.commit()',
  path.join(amux.home, 'amux.db'), sql], {encoding: 'utf8'});
const formPost = (p, form) => new Promise((resolve, reject) => {
  const data = Buffer.from(new URLSearchParams(form).toString());
  const r = https.request(amux.base + p, {method: 'POST', rejectUnauthorized: false, timeout: 15000,
    headers: {'Content-Type': 'application/x-www-form-urlencoded', 'Content-Length': data.length}}, res => {
    res.resume(); res.on('end', () => resolve({status: res.statusCode, headers: res.headers}));
  });
  r.on('error', reject); r.write(data); r.end();
});
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };
const DAY = 86400;
const nowS = () => Math.floor(Date.now() / 1000);

// A worker with its own folder (a git repo) and token.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-gov21-'));
const wdir = path.join(root, 'customers', 'wexus');
fs.mkdirSync(wdir, {recursive: true});
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
fs.writeFileSync(path.join(amux.home, 'sessions', 'wexus.env'), `CC_DIR=${wdir}\nCC_TAGS=customers\nCC_ARCHIVED=0\n`);
const wtoken = crypto.randomBytes(24).toString('hex');
fs.writeFileSync(path.join(amux.home, 'worker-tokens', 'wexus.sha256'),
  crypto.createHash('sha256').update(wtoken).digest('hex'), {mode: 0o600});
const WEXUS = {'X-Amux-Session': 'wexus', 'X-Amux-Worker-Token': wtoken};
const TRACE = crypto.randomBytes(16).toString('hex');
const SPAN = crypto.randomBytes(8).toString('hex');
const TP = `00-${TRACE}-${SPAN}-01`;
const ANN = 'ann@e2e.invalid', AUD = 'aud@e2e.invalid', OLD = 'old@e2e.invalid';

const member = async email => {
  const inv = await amux.req('POST', '/api/org/invites', {email, scope_level: 'global', grant_host_access: true}, 10000, OWNER());
  const tok = inv.body.token || String(inv.body.url || '').split('/invite/')[1];
  const acc = await formPost(`/invite/${tok}`, {email, name: email.split('@')[0]});
  return (acc.headers['set-cookie'] || []).map(c => c.split(';')[0]).find(c => c.startsWith('amux_member='));
};
const git = (...a) => execFileSync('git', ['-C', wdir, ...a], {encoding: 'utf8',
  env: {...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1'}});
// The CLI as the owner at their own terminal: no worker identity in the env.
const cli = (...args) => {
  const env = {...process.env, CC_HOME: amux.home, AMUX_API: amux.base, AMUX_URL: amux.base};
  for (const k of ['AMUX_SESSION', 'AMUX_WORKER', 'AMUX_WORKER_TOKEN', 'TMUX', 'TMUX_PANE']) delete env[k];
  const r = spawnSync('bash', [path.resolve('amux'), 'compliance', ...args], {encoding: 'utf8', env, cwd: root, timeout: 300000});
  return {status: r.status, out: (r.stdout || '') + (r.stderr || '')};
};
const finding = (review, rule, subject) => (review.findings || []).some(f => f.rule === rule && f.subject === subject);

let ok = false;
try {
  await restart({});

  // a. Activity with a trace, and a commit that carries it.
  const annCookie = await member(ANN);
  const audCookie = await member(AUD);
  check('a. two members accept and hold cookies', !!annCookie && !!audCookie, {annCookie, audCookie});
  const ANNH = {Cookie: annCookie}, AUDH = {Cookie: audCookie};
  for (const h of [{...WEXUS, traceparent: TP}, {...WEXUS, traceparent: TP}, ANNH]) {
    const r = await amux.req('POST', '/api/board', {title: 'card', status: 'todo', session: 'wexus'}, 10000, h);
    check('a. an action is answered', r.status > 0 && r.status < 500, r);
  }
  await sleep(500);
  const traced = db('select count(*) n from _amux_ledger where trace_id=? and actor=?', [TRACE, 'worker:wexus'])[0].n;
  check('a. the worker\'s traced actions are ledger rows', traced >= 2, db('select actor, trace_id from _amux_ledger order by seq desc limit 8'));
  git('init', '-q');
  fs.writeFileSync(path.join(wdir, 'note.md'), 'hello\n');
  git('add', 'note.md');
  git('-c', 'user.name=wexus (amux)', '-c', 'user.email=wexus+e2e00001@agents.noreply.amux.io', 'commit', '-q',
    '-m', 'agent work', '-m', `Amux-Trace: ${TRACE}\nOn-behalf-of: owner\nAmux-Host: e2e00001`);
  const sha = git('rev-parse', 'HEAD').trim();

  // b. Seeds the rules must flag: a member gone for 200 days, an unused SCIM
  // token, a standing approval with no expiry, a link pending for 200 days.
  const old = nowS() - 200 * DAY;
  db("insert into org_members (id, email, name, role, joined_at) values ('m_old', ?, 'Old', 'member', ?)", [OLD, old]);
  db("insert into scim_tokens (id, label, token_hash, created_by, created_at) values ('st_old', 'okta', ?, 'owner', ?)",
    [crypto.randomBytes(16).toString('hex'), old]);
  db("insert into standing_approvals (title, allowed, category, granted_at) values ('ship small fixes', 'x', 'decision', ?)", [old]);
  const linksFile = path.join(amux.home, 'public-links.json');
  // public-links.json is {"links": [...]} (static_files/public_links.rs load()).
  const links = fs.existsSync(linksFile) ? JSON.parse(fs.readFileSync(linksFile, 'utf8')) : {links: []};
  links.links = links.links || [];
  links.links.push({id: 'a'.repeat(32), kind: 'app', port: 3999, title: 'stale demo', created_by: 'wexus', created_at: old, approved_by: ''});
  fs.writeFileSync(linksFile, JSON.stringify(links), {mode: 0o600});

  let r = await amux.req('GET', '/api/compliance/access-review?stale_days=90', undefined, 60000, OWNER());
  check('b. the owner reads the access review', r.status === 200, r);
  const rv = r.body;
  for (const s of ['people', 'workers', 'apps', 'scim_tokens', 'standing_approvals', 'ledger']) {
    check(`b. ${s} was measured`, rv[s]?.measured === true, rv[s]);
  }
  check('b. the member gone 200 days is flagged inactive', finding(rv, 'person_inactive', 'member:' + OLD), rv.findings);
  check('b. the member who just acted is not', !finding(rv, 'person_inactive', 'member:' + ANN), rv.findings);
  check('b. the active worker is not flagged inactive', !finding(rv, 'worker_inactive', 'worker:wexus'), rv.findings);
  check('b. the unused SCIM token is flagged', finding(rv, 'scim_token_unused', 'scim_token:st_old'), rv.findings);
  check('b. the standing approval with no expiry is flagged',
    (rv.findings || []).some(f => f.rule === 'standing_approval_no_expiry'), rv.findings);
  check('b. the long-pending link is flagged', finding(rv, 'link_pending_long', 'app:' + 'a'.repeat(32)), rv.findings);
  const w = (rv.workers.items || []).find(x => x.worker === 'wexus');
  check('b. the worker carries its effective capabilities and last activity',
    w && w.capabilities?.worker === 'wexus' && w.capabilities?.measured === true && w.days_since_seen === 0, w);
  check('b. findings are counted by rule', Object.keys(rv.findings_by_rule || {}).length >= 4 && rv.n_considered === rv.findings.length, rv.findings_by_rule);

  // c. Who may read.
  const members = (await amux.req('GET', '/api/org/members', undefined, 10000, OWNER())).body;
  const audId = (Array.isArray(members) ? members : members.members || []).find(m => m.email === AUD)?.id;
  r = await amux.req('PATCH', `/api/org/members/${audId}`, {role: 'auditor'}, 10000, OWNER());
  check('c. the owner makes aud@ an auditor', r.status === 200 && r.body.role === 'auditor', r);
  for (const p of ['/api/compliance/access-review', `/api/compliance/timeline?trace=${TRACE}`]) {
    r = await amux.req('GET', p, undefined, 60000, AUDH);
    check(`c. the auditor reads ${p.split('?')[0]}`, r.status === 200, {status: r.status, body: r.body});
    for (const [who, h] of [['a plain member', ANNH], ['a worker', WEXUS]]) {
      r = await amux.req('GET', p, undefined, 60000, h);
      check(`c. ${who} is refused ${p.split('?')[0]}`, r.status === 403, {status: r.status, body: r.body});
    }
  }
  r = await amux.req('POST', '/api/compliance/access-review', {}, 10000, OWNER());
  check('c. the views take no writes', r.status === 405 || r.status === 404, r);

  // d. The timeline.
  r = await amux.req('GET', `/api/compliance/timeline?trace=${TRACE}`, undefined, 60000, OWNER());
  const tl = r.body;
  check('d. the trace timeline answers', r.status === 200 && tl.measured === true, r);
  const ledgerEv = (tl.events || []).filter(e => e.source === 'ledger');
  const commitEv = (tl.events || []).filter(e => e.source === 'commit');
  check('d. it holds the worker\'s traced ledger rows', ledgerEv.length >= 2 && ledgerEv.every(e => e.trace_id === TRACE), ledgerEv);
  check('d. it holds the commit that carries the trace', commitEv.some(e => e.commit === sha && e.trace_id === TRACE && e.on_behalf_of === 'owner'), tl.sources);
  const ts = (tl.events || []).map(e => e.ts);
  check('d. events are oldest first', ts.every((t, i) => i === 0 || ts[i - 1] <= t), ts);
  check('d. the chain verified beside it', tl.chain?.chain_ok === true, tl.chain);
  check('d. each source says it measured', tl.sources?.ledger?.measured === true && tl.sources?.commits?.measured === true, tl.sources);
  r = await amux.req('GET', '/api/compliance/timeline?worker=wexus', undefined, 60000, OWNER());
  check('d. by worker it finds the agent-authored commit',
    r.status === 200 && (r.body.events || []).some(e => e.source === 'commit' && e.commit === sha), r.body.sources);
  r = await amux.req('GET', '/api/compliance/timeline', undefined, 10000, OWNER());
  check('d. a timeline with no filter is refused', r.status === 400 && r.body.code === 'timeline_needs_a_filter', r);
  r = await amux.req('GET', '/api/compliance/timeline?trace=NOT-A-TRACE', undefined, 10000, OWNER());
  check('d. a malformed trace is refused', r.status === 400 && r.body.code === 'bad_trace_id', r);

  // e. The evidence pack, through the CLI.
  const outDir = path.join(root, 'pack');
  let c = cli('evidence', '--out', outDir);
  check('e. evidence exits 0', c.status === 0, c);
  const mf = JSON.parse(fs.readFileSync(path.join(outDir, 'manifest.json'), 'utf8'));
  check('e. verdict complete with the chain verified', mf.verdict === 'complete' && mf.ledger_chain_ok === true && mf.missing.length === 0, mf);
  check('e. ten evidence files, each answered 200', mf.items.length === 10 && mf.items.every(i => i.http_status === 200), mf.items);
  for (const i of mf.items) {
    const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(outDir, i.file))).digest('hex');
    check(`e. ${i.file} matches its manifest hash`, got === i.sha256 && i.bytes > 0, {i, got});
  }
  check('e. the CSV export carries the chain verdict header', mf.items.find(i => i.file === 'audit-export.csv')?.chain_ok_header === 'true', mf.items);
  check('e. CONTROLS.md maps controls to files', /Audit logging/.test(fs.readFileSync(path.join(outDir, 'CONTROLS.md'), 'utf8')), null);
  c = cli('evidence', '--out', outDir);
  check('e. an existing pack is never overwritten', c.status !== 0 && /never overwritten/.test(c.out), c);
  c = cli('access-review');
  check('e. the CLI access review prints the findings', c.status === 0 && /scim_token_unused/.test(c.out) && /finding/.test(c.out), c);
  c = cli('timeline', '--trace', TRACE);
  check('e. the CLI timeline prints the commit and a verified chain', c.status === 0 && c.out.includes(sha.slice(0, 9)) && /chain: ok/.test(c.out), c);

  // f. Tamper with a row behind the triggers: the pack must say so.
  const target = db('select event_id from _amux_ledger where actor=? order by seq limit 1', ['worker:wexus'])[0].event_id;
  dbScript(`DROP TRIGGER IF EXISTS amux_ledger_no_update; DROP TRIGGER IF EXISTS amux_ledger_erase_only;
            UPDATE _amux_ledger SET actor='owner' WHERE event_id='${target}';`);
  c = cli('evidence', '--out', path.join(root, 'pack-tampered'));
  const mf2 = JSON.parse(fs.readFileSync(path.join(root, 'pack-tampered', 'manifest.json'), 'utf8'));
  check('f. a tampered ledger makes the pack incomplete, exit 3', c.status === 3 && mf2.verdict === 'incomplete' && mf2.ledger_chain_ok === false, {c, mf2});
  r = await amux.req('GET', `/api/compliance/timeline?trace=${TRACE}`, undefined, 60000, OWNER());
  check('f. the timeline\'s chain says not verified', r.body.chain?.chain_ok === false, r.body.chain);
  c = cli('timeline', '--trace', TRACE);
  check('f. the CLI says NOT VERIFIED', /NOT VERIFIED/.test(c.out), c);

  // g. Counters.
  const cc = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.compliance || {};
  check('g. the counters count reads and refusals',
    cc.measured === true && cc.counts?.access_reviews >= 3 && cc.counts?.timelines >= 4 && cc.counts?.refused >= 4, cc);
  ok = true;
} finally {
  const receipt = {scenario: 'compliance', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
  fs.rmSync(root, {recursive: true, force: true});
}
process.exit(ok ? 0 : 1);
