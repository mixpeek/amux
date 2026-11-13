#!/usr/bin/env node
// END-TO-END (governance PR 3): loopback is not a credential.
//
// A private amux (own home, port and tmux socket, owner token on disk, as a
// normal install has), reached over loopback, where every request used to be
// let in without a credential:
//   a. the diagnostic (GET /api/contract/counters -> loopback) reports mode,
//      measured, n_considered and that this server can enforce;
//   b. report mode (the default) lets an uncredentialed local request through,
//      counts it and names the caller, logging it once;
//   c. after a restart with AMUX_LOOPBACK_TRUST=enforce the same request is
//      refused 401 loopback_not_trusted, and nothing that should work stops:
//      the owner bearer (what the dashboard sends), the owner's own `amux` CLI,
//      a worker lane's `amux` CLI (its minted token), a hook's read (the
//      shipped boundary-guard), and a shell schedule firing (its run token);
//      the dashboard shell and public paths still load with no credential;
//   d. AMUX_LOOPBACK_TRUST=off trusts loopback again and counts nothing.
// The dashboard under enforce is e2e/loopback-not-trusted.spec.ts (the whole
// Playwright suite already runs with AMUX_RS_NO_LOOPBACK_BYPASS=1, which now
// means enforce).
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/loopback-not-trusted.mjs
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', FAKE_CLAUDE_ENV_KEYS: 'AMUX_WORKER_TOKEN', AMUX_RS_SCHEDULER: '1',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const OWNER = () => ({Authorization: 'Bearer ' + fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim()});
const CURL = {'User-Agent': 'curl/8.5.0'};
const loopback = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.loopback || {};
const ownerCli = (...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, CC_HOME: amux.home},
  encoding: 'utf8', timeout: 30000,
});
const laneCli = (lane, tok, ...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, AMUX_SESSION: lane, AMUX_WORKER_TOKEN: tok},
  encoding: 'utf8', timeout: 30000,
});
// The shipped boundary-guard's read of /api/resources, as the hook runs it in a lane.
const hookRead = (lane, tok) => execFileSync('python3', ['-c', `
import importlib.util, sys
spec = importlib.util.spec_from_file_location("bg", sys.argv[1])
bg = importlib.util.module_from_spec(spec); spec.loader.exec_module(bg)
print(repr(bg.my_usage(sys.argv[2])))
`, path.join(repo, 'scripts', 'hooks', 'boundary-guard.py'), lane], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_SESSION: lane, AMUX_WORKER_TOKEN: tok}, encoding: 'utf8', timeout: 30000,
});
const logLines = verdict => fs.readFileSync(amux.serverLog, 'utf8').split('\n').filter(l => l.includes(verdict));
const lane = 'lb-lane';
const token = () => amux.tmux('show-environment', '-t', 'amux-' + lane, 'AMUX_WORKER_TOKEN').trim().split('=').slice(1).join('=');

let ok = false;
try {
  // A worker lane with a minted token (governance PR 1).
  const dir = path.join(amux.root, lane);
  fs.mkdirSync(dir);
  const created = await amux.req('POST', '/api/sessions', {name: lane, dir, start: false}, 10000, OWNER());
  check('a worker lane is created', created.status === 201, created);
  fs.appendFileSync(path.join(amux.home, 'sessions', lane + '.env'), '\nAMUX_BOARD_FORCE_ADHERENCE=1\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n');
  const started = await amux.req('POST', `/api/sessions/${lane}/start`, undefined, 10000, OWNER());
  check('it starts', started.status < 300, started);
  await waitFor('the lane token', () => /^[0-9a-f]{64}$/.test(token()) || null, 30000).catch(() => null);
  const tok = token();
  check('the lane holds its minted token', /^[0-9a-f]{64}$/.test(tok), tok.length);

  // a. The diagnostic.
  let c = await loopback();
  check('a. report is the default mode', c.mode === 'report', c);
  check('a. it says it measured and can enforce here', c.measured === true && typeof c.n_considered === 'number'
    && c.owner_token_configured === true && c.enforceable === true, c);

  // b. Report: a bare local request still works, is counted and named.
  const before = c.requests;
  const r1 = await amux.req('POST', '/api/board', {title: 'loopback e2e: bare curl, report', type: 'chore', status: 'backlog'}, 10000, CURL);
  check('b. report mode lets an uncredentialed local write through', r1.status === 201, r1);
  c = await loopback();
  check('b. it was counted would_refuse', c.requests.would_refuse > before.would_refuse, {before, after: c.requests});
  check('b. the caller is named', (c.uncredentialed_callers || []).some(x => x.caller.startsWith('curl /api/board lane=- peer=loopback')), c.uncredentialed_callers);
  await waitFor('would-refuse log line', () => logLines('loopback_would_be_refused').length > 0 || null, 10000);
  await amux.req('POST', '/api/board', {title: 'loopback e2e: bare curl again', type: 'chore', status: 'backlog'}, 10000, CURL);
  check('b. the same caller is logged once', logLines('loopback_would_be_refused').filter(l => l.includes('curl /api/board')).length === 1);
  const fromLane = laneCli(lane, tok, 'board', 'add', 'loopback e2e: lane CLI, report');
  check('b. the lane CLI works and is counted as a worker token', /[A-Z]+-\d+/.test(fromLane) && (await loopback()).requests.worker_token > c.requests.worker_token, fromLane);

  // c. Enforce.
  await amux.down();
  amux.env.AMUX_LOOPBACK_TRUST = 'enforce';
  await amux.up();
  c = await loopback();
  check('c. restarted in enforce mode (the owner bearer reads the diagnostic)', c.mode === 'enforce', c);
  const r2 = await amux.req('POST', '/api/board', {title: 'loopback e2e: bare curl, enforce', type: 'chore', status: 'backlog'}, 10000, CURL);
  check('c. an uncredentialed local write is refused 401', r2.status === 401 && r2.body.reason === 'loopback_not_trusted', r2);
  const r3 = await amux.req('GET', '/api/sessions', undefined, 10000, CURL);
  check('c. an uncredentialed local read is refused too', r3.status === 401, r3.status);
  const forged = await amux.req('GET', '/api/sessions', undefined, 10000, {...CURL, 'X-Amux-Session': lane, 'X-Amux-Worker-Token': 'f'.repeat(64)});
  check('c. a wrong worker token is refused', forged.status === 401, forged.status);
  const r4 = await amux.req('POST', '/api/board', {title: 'loopback e2e: owner bearer, enforce', type: 'chore', status: 'backlog'}, 10000, OWNER());
  check('c. the owner bearer (what the dashboard sends) still writes', r4.status === 201, r4);
  const shell = await amux.req('GET', '/', undefined, 10000, CURL);
  check('c. the dashboard shell loads with no credential', shell.status === 200, shell.status);
  const health = await amux.req('GET', '/health', undefined, 10000, CURL);
  check('c. /health stays public', health.status === 200, health.status);
  const ownerOut = ownerCli('board', 'add', 'loopback e2e: owner CLI, enforce');
  check("c. the owner's own CLI still writes", /[A-Z]+-\d+/.test(ownerOut), ownerOut);
  const laneOut = laneCli(lane, tok, 'board', 'add', 'loopback e2e: lane CLI, enforce');
  check("c. a worker's CLI still writes with its token", /[A-Z]+-\d+/.test(laneOut), laneOut);
  const laneRead = laneCli(lane, tok, 'get', 'api/board');
  check("c. a worker's CLI still reads", laneRead.trim().startsWith('[') || laneRead.trim().startsWith('{'), laneRead.slice(0, 200));
  const beforeHook = (await loopback()).requests;
  hookRead(lane, tok);
  const afterHook = (await loopback()).requests;
  check("c. the boundary-guard hook's read carries the lane token (counted, not refused)",
    afterHook.worker_token > beforeHook.worker_token && afterHook.refused === beforeHook.refused, {beforeHook, afterHook});
  const sched = await amux.req('POST', '/api/schedules', {title: 'loopback e2e', session: lane, kind: 'shell',
    command: `AMUX_URL=${amux.base} bash ${path.join(repo, 'amux')} board add 'loopback e2e: from a schedule'`,
    schedule_expr: 'daily at 3am'}, 10000, OWNER());
  const sid = sched.body.id || sched.body.schedule?.id;
  check('c. a shell schedule is created', !!sid, sched);
  const fired = await amux.req('POST', `/api/schedules/${sid}/run`, undefined, 10000, OWNER());
  check('c. it fires', fired.status < 300, fired);
  const board = await waitFor('the schedule wrote its card', async () => {
    const b = await amux.req('GET', '/api/board', undefined, 10000, OWNER());
    const rows = Array.isArray(b.body) ? b.body : (b.body.items || b.body.issues || []);
    return rows.find(r => (r.title || '').includes('from a schedule')) || null;
  }, 30000).catch(() => null);
  check('c. a scheduled run still writes (its run token)', !!board, (await amux.req('GET', `/api/schedules/${sid}/output`, undefined, 10000, OWNER())).body);
  c = await loopback();
  check('c. refused requests are counted', c.requests.refused >= 3, c.requests);
  await waitFor('refused log line', () => logLines('loopback_refused').length > 0 || null, 10000);

  // d. Off.
  await amux.down();
  amux.env.AMUX_LOOPBACK_TRUST = 'off';
  await amux.up();
  const r5 = await amux.req('POST', '/api/board', {title: 'loopback e2e: bare curl, off', type: 'chore', status: 'backlog'}, 10000, CURL);
  c = await loopback();
  check('d. AMUX_LOOPBACK_TRUST=off trusts loopback and counts nothing', r5.status === 201 && c.mode === 'off' && c.requests.considered === 0, {r5, c});
  ok = true;
} finally {
  const receipt = {scenario: 'loopback-not-trusted', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
