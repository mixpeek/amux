#!/usr/bin/env node
// END-TO-END (governance PR 14): every delivered message records where its
// instruction came from, derived from the proven sender, never the text.
//
// A private amux with workers prov-a and prov-b (group product) and an
// isolated worker prov-iso, a Global member M, and a Slack bridge with a test
// signing secret. Checks:
//   a. the owner's send: source owner, no origin line in the worker's text;
//   b. M's send: source member M, and the worker's text starts with the
//      server-verified [amux-origin: member:M ...] stamp;
//   c. A's send to B through the shipped CLI: source worker prov-a;
//   d. M's send to the isolated worker: the row records member M, the text
//      reaching the worker carries no stamp (A9: metadata only);
//   e. a schedule firing (real cron path): source schedule SCHED-n;
//   f. a signed Slack message relayed into A's thread: relayed_from names the
//      platform and sender, source webhook;
//   g. GET /api/workers/{name}/history shows it, measured; a worker cannot
//      read another worker's; GET /api/trace/{id} carries the source fields;
//      the counters report it;
//   h. AMUX_INSTRUCTION_PROVENANCE=off: no stamp, no source recorded.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/instruction-provenance.mjs
import fs from 'node:fs';
import path from 'node:path';
import https from 'node:https';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
const SLACK_SECRET = 'e2e-slack-signing-secret';
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_ISOLATED: '0', AMUX_RS_SCHEDULER: '0', AMUX_BOARD_DRIVE_SECS: '0',
  AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0', AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
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
const token = lane => amux.tmux('show-environment', '-t', 'amux-' + lane, 'AMUX_WORKER_TOKEN').trim().split('=').slice(1).join('=');
const cli = (lane, tok, ...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, AMUX_SESSION: lane, AMUX_WORKER_TOKEN: tok, CC_HOME: amux.home},
  encoding: 'utf8', timeout: 30000,
});
const post = (p, body, headers = {}) => new Promise((resolve, reject) => {
  const data = Buffer.from(typeof body === 'string' ? body : new URLSearchParams(body).toString());
  const r = https.request(amux.base + p, {method: 'POST', rejectUnauthorized: false, timeout: 15000,
    headers: {'Content-Type': typeof body === 'string' ? 'application/json' : 'application/x-www-form-urlencoded',
      'Content-Length': data.length, ...headers}}, res => {
    res.resume(); res.on('end', () => resolve({status: res.statusCode, headers: res.headers}));
  });
  r.on('error', reject); r.write(data); r.end();
});
const delivered = (lane, text) => db('select * from cmd_history where session=? and text like ? order by id desc', [lane, '%' + text + '%'])[0];
const reached = text => amux.fakeLog().filter(x => (x.text || '').includes(text)).map(x => x.text);
// The fake CLI logs each submitted line as its own entry, so a stamp is the
// line just before the message's own.
const lineBefore = text => {
  const log = amux.fakeLog();
  const i = log.findIndex(x => (x.text || '').includes(text));
  return i > 0 ? (log[i - 1].text || '') : '';
};
const M = 'm@e2e.invalid';

let ok = false;
try {
  fs.appendFileSync(path.join(amux.home, 'server.env'), `\nSLACK_SIGNING_SECRET=${SLACK_SECRET}\nSLACK_BOT_TOKEN=xoxb-e2e-not-real\n`);
  for (const [name, iso] of [['prov-a', 0], ['prov-b', 0], ['prov-iso', 1]]) {
    const dir = path.join(amux.root, name); fs.mkdirSync(dir);
    const r = await amux.req('POST', '/api/sessions', {name, dir, start: false}, 15000, OWNER());
    check('setup: worker ' + name, r.status === 201, r);
    fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'),
      `\nCC_TAGS=product\nCC_ISOLATED=${iso}\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nAMUX_BOARD_FORCE_ADHERENCE=1\n`);
    const s = await amux.req('POST', `/api/sessions/${name}/start`, undefined, 15000, OWNER());
    check('setup: started ' + name, s.status < 300, s);
    await waitFor('launch ' + name, () => amux.fakeLog().some(x => x.event === 'launch' && (x.argv || []).includes(name)), 30000);
  }
  const inv = await amux.req('POST', '/api/org/invites', {email: M, scope_level: 'global', scope_name: '', grant_host_access: true}, 10000, OWNER());
  const tok = inv.body.token || String(inv.body.url || '').split('/invite/')[1];
  const acc = await post(`/invite/${tok}`, {email: M, name: 'M'});
  const cookie = (acc.headers['set-cookie'] || []).map(c => c.split(';')[0]).find(c => c.startsWith('amux_member='));
  check('setup: member M holds a cookie', !!cookie, acc);
  const AS_M = {Cookie: cookie};

  // a. The owner.
  const o = await amux.req('POST', '/api/workers/prov-a/send', {text: 'prov e2e: owner asks'}, 30000, OWNER());
  check('a. owner send accepted', o.status < 300, o);
  await waitFor('owner text reaches A', () => reached('prov e2e: owner asks').length, 30000);
  const rowO = await waitFor('owner row', () => delivered('prov-a', 'owner asks'), 10000);
  check('a. source is the owner', rowO.source_kind === 'owner' && rowO.source_ref === 'owner' && !rowO.relayed_from, rowO);
  check("a. the owner's words carry no origin line", !lineBefore('prov e2e: owner asks').includes('[amux-origin')
    && !reached('prov e2e: owner asks').some(t => t.includes('[amux-origin')), lineBefore('prov e2e: owner asks'));

  // b. A member.
  const m = await amux.req('POST', '/api/workers/prov-a/send', {text: 'prov e2e: member asks'}, 30000, AS_M);
  check('b. member send accepted', m.status < 300, m);
  await waitFor('member text reaches A', () => reached('prov e2e: member asks')[0], 30000);
  check('b. the worker reads a server-verified line naming the member',
    lineBefore('prov e2e: member asks').startsWith(`[amux-origin: member:${M} — server-verified`), lineBefore('prov e2e: member asks'));
  const rowM = await waitFor('member row', () => delivered('prov-a', 'member asks'), 10000);
  check('b. source is member M', rowM.source_kind === 'member' && rowM.source_ref === M, rowM);

  // c. Worker to worker.
  cli('prov-a', token('prov-a'), 'send', 'prov-b', 'prov e2e: A asks B');
  await waitFor('B receives', () => reached('prov e2e: A asks B').length, 30000);
  const rowW = await waitFor('B row', () => delivered('prov-b', 'A asks B'), 10000);
  check('c. source is worker prov-a, proven', rowW.source_kind === 'worker' && rowW.source_ref === 'prov-a', rowW);

  // d. Isolated: metadata only.
  const i = await amux.req('POST', '/api/workers/prov-iso/send', {text: 'prov e2e: member asks iso'}, 30000, AS_M);
  check('d. member send to the isolated worker accepted', i.status < 300, i);
  const iText = await waitFor('isolated text', () => reached('prov e2e: member asks iso')[0], 30000);
  check('d. the isolated worker reads no stamp', !iText.includes('[amux-origin')
    && !lineBefore('prov e2e: member asks iso').includes('[amux-origin'), [lineBefore('prov e2e: member asks iso'), iText]);
  const rowI = await waitFor('isolated row', () => delivered('prov-iso', 'member asks iso'), 10000);
  check('d. ...but its row records member M', rowI.source_kind === 'member' && rowI.source_ref === M, rowI);

  // f. A Slack message relayed into A's thread.
  db("insert into bridge_links (platform, conversation, kind, conv, last_post_id, linked_by, linked_at) values ('slack','C0E2E','group','w:prov-a',0,'owner',0)");
  const ev = JSON.stringify({type: 'event_callback', event: {type: 'message', channel: 'C0E2E', channel_type: 'channel',
    user: 'U0E2E', text: '<@prov-a> prov e2e: slack asks', ts: '1700000000.000100'}});
  const ts = String(Math.floor(Date.now() / 1000));
  const sig = 'v0=' + crypto.createHmac('sha256', SLACK_SECRET).update(`v0:${ts}:${ev}`).digest('hex');
  const hook = await post('/api/bridge/hook/slack', ev, {'X-Slack-Request-Timestamp': ts, 'X-Slack-Signature': sig});
  check('f. the signed Slack event is accepted', hook.status < 300, hook);
  const rowS = await waitFor('relayed row', () => delivered('prov-a', 'slack asks'), 30000);
  check('f. relayed_from names Slack and the sender', rowS.relayed_from === 'webhook:slack:U0E2E'
    && rowS.source_kind === 'webhook' && rowS.source_ref === 'webhook:slack:U0E2E', rowS);

  // g. Reading it back.
  const h = await amux.req('GET', '/api/workers/prov-a/history?limit=20', undefined, 10000, OWNER());
  const kinds = (h.body.items || []).map(x => x.source_kind);
  check('g. the history view is measured and shows each source', h.status === 200 && h.body.measured === true
    && ['owner', 'member', 'webhook'].every(k => kinds.includes(k)), h.body);
  const hb = await amux.req('GET', '/api/workers/prov-a/history', undefined, 10000,
    {'X-Amux-Session': 'prov-b', 'X-Amux-Worker-Token': token('prov-b')});
  check("g. a worker cannot read another worker's history", hb.status === 403, hb);
  const own = await amux.req('GET', '/api/workers/prov-b/history', undefined, 10000,
    {'X-Amux-Session': 'prov-b', 'X-Amux-Worker-Token': token('prov-b')});
  check('g. ...but can read its own', own.status === 200 && (own.body.items || []).some(x => x.source_kind === 'worker'), own.body);
  if (rowO.trace_id) {
    const t = (await amux.req('GET', `/api/trace/${rowO.trace_id}`, undefined, 10000, OWNER())).body;
    const flat = n => [n, ...(n.children || []).flatMap(flat)];
    const node = (t.tree || []).flatMap(flat).find(n => n.id === `MSG-${rowO.id}`);
    check("g. the trace view carries the delivery's source", node && node.source_kind === 'owner', node);
  }

  // e. A schedule firing on the real cron path.
  const sched = await amux.req('POST', '/api/schedules', {title: 'prov e2e schedule', session: 'prov-a',
    command: 'prov e2e: scheduled ask', schedule_expr: 'daily at 3am', enabled: 0}, 10000, OWNER());
  const sid = sched.body.id || sched.body.schedule?.id;
  check('e. a schedule is created', !!sid, sched);
  await amux.down();
  db("update schedules set enabled=1, sched_type='once', next_run='2020-01-01T00:00' where id=?", [sid]);
  amux.env.AMUX_RS_SCHEDULER = '1';
  await amux.up();
  const rowC = await waitFor('scheduled row', () => delivered('prov-a', 'scheduled ask'), 60000);
  check('e. source is the schedule', rowC.source_kind === 'schedule' && rowC.source_ref === sid, rowC);
  const c = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.instruction_provenance || {};
  check('g. the counters report it, measured', c.enabled === true && c.measured === true && c.recorded >= 1, c);

  // h. Off.
  await amux.down();
  amux.env.AMUX_INSTRUCTION_PROVENANCE = 'off';
  amux.env.AMUX_RS_SCHEDULER = '0';
  await amux.up();
  const off = await amux.req('POST', '/api/workers/prov-a/send', {text: 'prov e2e: member asks with it off'}, 30000, AS_M);
  check('h. off: member send accepted', off.status < 300, off);
  const offText = await waitFor('off text', () => reached('prov e2e: member asks with it off')[0], 30000);
  check('h. off: no stamp', !offText.includes('[amux-origin')
    && !lineBefore('prov e2e: member asks with it off').includes('[amux-origin'), [lineBefore('prov e2e: member asks with it off'), offText]);
  const rowOff = await waitFor('off row', () => delivered('prov-a', 'with it off'), 10000);
  check('h. off: no source recorded', !rowOff.source_kind && !rowOff.source_ref, rowOff);
  ok = true;
} finally {
  const receipt = {scenario: 'instruction-provenance', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
