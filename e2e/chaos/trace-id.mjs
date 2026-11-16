#!/usr/bin/env node
// END-TO-END (governance PR 8): one trace id per unit of work, minted at the
// origin and carried through every hop.
//
// A private amux (own home, port and tmux socket, fake provider CLIs on PATH)
// with two workers in one group. Checks:
//   a. the owner's send to worker A carries no traceparent, so the server
//      mints one at the origin and names it on the response;
//   b. A's delivered message records the trace as metadata (a child span of
//      the send, source `delivered`), the message text has no trace in it, and
//      <home>/trace/A.current holds the delivery's traceparent;
//   c. calls A makes afterwards through the shipped `amux` CLI (a board card, a
//      send to worker B) share the trace and descend from A's delivery span;
//      B's delivered message descends from A's send;
//   d. a commit A makes in a git repo gets an `Amux-Trace` trailer from the
//      prepare-commit-msg hook;
//   e. a connector token mint A makes with that traceparent lands as a policy
//      receipt in the ledger, in the same trace;
//   f. GET /api/trace/{id} returns all of it as one tree with the owner's
//      send at the root, measured, and names the sources it consulted;
//   g. a schedule firing (the real cron path) mints its own trace: the fire is
//      the root, A's delivery its child;
//   h. AMUX_TRACE=off: no traceparent on responses, no trace on deliveries.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/trace-id.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
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
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-I', '-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const token = lane => amux.tmux('show-environment', '-t', 'amux-' + lane, 'AMUX_WORKER_TOKEN').trim().split('=').slice(1).join('=');
// The shipped CLI, with the environment a lane gives it. No TRACEPARENT: the
// CLI must find the trace in the worker's file on its own.
const cli = (lane, tok, ...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, AMUX_SESSION: lane, AMUX_WORKER_TOKEN: tok,
    // A real lane's ~/.amux is the server's home; here the server's home is private.
    CC_HOME: amux.home},
  encoding: 'utf8', timeout: 30000,
});
const delivered = (lane, text) => db('select * from cmd_history where session=? and text like ? order by id desc', [lane, '%' + text + '%'])[0];
const ledger = async q => (await amux.req('GET', '/api/ledger?limit=1000&' + q)).body.events || [];
const TP = /^00-([0-9a-f]{32})-([0-9a-f]{16})-01$/;

let ok = false;
try {
  for (const name of ['trace-a', 'trace-b']) {
    const dir = path.join(amux.root, name); fs.mkdirSync(dir);
    const r = await amux.req('POST', '/api/sessions', {name, dir, start: false});
    check('worker created ' + name, r.status === 201, r);
    fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'),
      '\nCC_TAGS=product\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nAMUX_BOARD_FORCE_ADHERENCE=1\n');
    const s = await amux.req('POST', `/api/sessions/${name}/start`);
    check('worker started ' + name, s.status < 300, s);
    await waitFor('provider launch ' + name, () => amux.fakeLog().some(x => x.event === 'launch' && (x.argv || []).includes(name)), 30000);
  }

  // a. Origin.
  const send = await amux.req('POST', '/api/workers/trace-a/send', {text: 'trace e2e: owner asks A'}, 30000);
  check('a. the owner sends to A', send.status < 300, send);
  const m = TP.exec(send.headers.traceparent || '');
  check('a. the response names a minted trace', !!m, send.headers);
  const [, T, ownerSpan] = m;
  await waitFor('A receives the message', () => amux.fakeLog().some(x => x.text?.includes('trace e2e: owner asks A')), 30000);

  // b. Into the worker, as metadata.
  const rowA = await waitFor('A delivery row', () => delivered('trace-a', 'owner asks A'), 10000);
  check('b. A delivery carries the trace as a child of the send', rowA.trace_id === T && rowA.parent_span_id === ownerSpan
    && /^[0-9a-f]{16}$/.test(rowA.span_id || '') && rowA.span_id !== ownerSpan && rowA.trace_source === 'delivered', rowA);
  check('b. the message text carries no trace id', !rowA.text.includes(T)
    && !amux.fakeLog().some(x => (x.text || '').includes(T)), rowA.text);
  const fileA = fs.readFileSync(path.join(amux.home, 'trace', 'trace-a.current'), 'utf8').split('\n');
  check("b. A's trace file holds the delivery's traceparent", fileA[0] === `00-${T}-${rowA.span_id}-01` && fileA[1] === 'source=delivered', fileA);

  // c. A's own calls through the CLI.
  const tokA = token('trace-a');
  check('A has a worker token', /^[0-9a-f]{64}$/.test(tokA), tokA.length);
  const card = cli('trace-a', tokA, 'board', 'add', 'trace e2e: card from A');
  check('c. A files a card through the CLI', /[A-Z]+-\d+/.test(card), card);
  const cardRow = (await ledger(`trace_id=${T}`)).find(e => e.action === 'POST /api/board');
  check('c. ...in the same trace, descending from A delivery', cardRow && cardRow.parent_span_id === rowA.span_id
    && cardRow.trace_source === 'inherited' && cardRow.actor === 'worker:trace-a', cardRow);
  cli('trace-a', tokA, 'send', 'trace-b', 'trace e2e: A asks B');
  await waitFor('B receives the message', () => amux.fakeLog().some(x => x.text?.includes('trace e2e: A asks B')), 30000);
  const sendRow = (await ledger(`trace_id=${T}`)).find(e => /\/trace-b\/send$/.test(e.action || '') || (e.resource || '').includes('trace-b'));
  check("c. A's send to B is in the trace, a child of A delivery", sendRow && sendRow.parent_span_id === rowA.span_id, sendRow);
  const rowB = await waitFor('B delivery row', () => delivered('trace-b', 'A asks B'), 10000);
  check("c. B's delivery descends from A's send", rowB.trace_id === T && rowB.parent_span_id === sendRow.span_id, {rowB, sendRow});

  // d. A commit.
  const gitDir = path.join(amux.root, 'trace-repo');
  fs.mkdirSync(gitDir);
  const g = (...args) => execFileSync('git', args, {cwd: gitDir, encoding: 'utf8',
    env: {...amux.env, AMUX_SESSION: 'trace-a', AMUX_HOME: amux.home}});
  g('init', '-q');
  fs.copyFileSync(path.join(repo, 'scripts/git-hooks/prepare-commit-msg'), path.join(gitDir, '.git/hooks/prepare-commit-msg'));
  fs.chmodSync(path.join(gitDir, '.git/hooks/prepare-commit-msg'), 0o755);
  fs.writeFileSync(path.join(gitDir, 'f.txt'), 'x\n');
  g('add', 'f.txt');
  g('commit', '-q', '-m', 'trace e2e: a commit from A');
  const msg = g('log', '-1', '--format=%B');
  check('d. the commit carries Amux-Trace', msg.includes(`Amux-Trace: ${T}`) && msg.includes('Amux-Session: trace-a'), msg);

  // e. A connector token mint (its receipt lands in the ledger).
  const tp = fs.readFileSync(path.join(amux.home, 'trace', 'trace-a.current'), 'utf8').split('\n')[0];
  await amux.req('POST', '/api/connectors/google-gmail/token', {}, 10000,
    {'X-Amux-Session': 'trace-a', 'X-Amux-Worker-Token': tokA, traceparent: tp});
  const mint = (await ledger(`trace_id=${T}`)).find(e => e.kind === 'policy' && e.action === 'connector_token_mint');
  const mintReq = (await ledger(`trace_id=${T}`)).find(e => e.action === 'POST /api/connectors/google-gmail/token');
  check('e. the mint receipt is in the trace, under its request', mint && mintReq && mint.parent_span_id === mintReq.span_id
    && mintReq.parent_span_id === rowA.span_id, {mint, mintReq});

  // f. The tree.
  const tree = (await amux.req('GET', `/api/trace/${T}`)).body;
  check('f. the trace is measured and names its sources', tree.measured === true && tree.n_considered >= 6
    && tree.sources.some(s => s.name === 'ledger' && s.consulted) && tree.sources.some(s => s.name === 'cmd_history' && s.consulted), tree);
  const roots = tree.tree.filter(n => !n.orphan);
  check("f. the owner's send is the one root", roots.length === 1 && roots[0].span_id === ownerSpan && /trace-a\/send/.test(roots[0].action || ''), tree.tree);
  const flat = n => [n, ...(n.children || []).flatMap(flat)];
  const all = flat(roots[0]);
  const deliveryA = all.find(n => n.id === `MSG-${rowA.id}`);
  check("f. A's delivery hangs under the send, and A's card, send and mint under the delivery", deliveryA
    && deliveryA.children.some(n => n.action === 'POST /api/board')
    && deliveryA.children.some(n => n.span_id === sendRow.span_id)
    && deliveryA.children.some(n => n.span_id === mintReq.span_id), deliveryA);
  check("f. B's delivery hangs under A's send", all.find(n => n.span_id === sendRow.span_id)?.children.some(n => n.id === `MSG-${rowB.id}`), all.map(n => n.id));
  const bad = (await amux.req('GET', '/api/trace/not-hex')).status;
  check('f. a malformed trace id is a 400', bad === 400, bad);

  // g. A schedule firing on the real cron path.
  const sched = await amux.req('POST', '/api/schedules', {title: 'trace e2e schedule', session: 'trace-a',
    command: 'trace e2e: scheduled ask', schedule_expr: 'daily at 3am', enabled: 0});
  const sid = sched.body.id || sched.body.schedule?.id;
  check('g. a schedule for A is created', !!sid, sched);
  await amux.down();
  db("update schedules set enabled=1, sched_type='once', next_run='2020-01-01T00:00' where id=?", [sid]);
  amux.env.AMUX_RS_SCHEDULER = '1';
  await amux.up();
  const rowS = await waitFor('the scheduled delivery', () => delivered('trace-a', 'scheduled ask'), 60000);
  check('g. the scheduled delivery has its own trace', /^[0-9a-f]{32}$/.test(rowS.trace_id || '') && rowS.trace_id !== T, rowS);
  const fire = (await ledger(`trace_id=${rowS.trace_id}`)).find(e => e.kind === 'schedule');
  check('g. ...rooted at the recorded firing, minted', fire && fire.resource === `schedule:${sid}` && fire.trace_source === 'minted'
    && !fire.parent_span_id && rowS.parent_span_id === fire.span_id, {fire, rowS});
  const st = (await amux.req('GET', `/api/trace/${rowS.trace_id}`)).body;
  check('g. its tree has the firing at the root and the delivery under it', st.tree.length === 1 && st.tree[0].kind === 'schedule'
    && st.tree[0].children.some(n => n.id === `MSG-${rowS.id}`), st.tree);
  const counters = (await amux.req('GET', '/api/contract/counters')).body.trace || {};
  check('g. the counters report traces, measured', counters.enabled === true && counters.measured === true
    && counters.minted >= 1 && counters.trace_propagation_failed === 0, counters);

  // h. Off.
  await amux.down();
  amux.env.AMUX_TRACE = 'off';
  amux.env.AMUX_RS_SCHEDULER = '0';
  await amux.up();
  const off = await amux.req('POST', '/api/workers/trace-a/send', {text: 'trace e2e: with tracing off'}, 30000);
  check('h. off: no traceparent on the response', off.status < 300 && !off.headers.traceparent, off.headers);
  const rowOff = await waitFor('the untraced delivery', () => delivered('trace-a', 'with tracing off'), 10000);
  check('h. off: the delivery carries no trace', !rowOff.trace_id && !rowOff.span_id, rowOff);
  ok = true;
} finally {
  const receipt = {scenario: 'trace-id', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
