#!/usr/bin/env node
// END-TO-END (governance PR 1): every worker lane gets an identity token by
// default, not only lanes on the orchestration contract.
//
// A private amux (own home, port and tmux socket, fake provider CLIs on PATH)
// launches one worker per provider with NO contract settings, then proves:
//   a. each launch's environment carries AMUX_WORKER_TOKEN (claude, codex,
//      gemini; the harness makes workers isolated, so isolated lanes too);
//   b. the real `amux` CLI run with that lane's environment makes a mutating
//      call that the server counts as `valid`, with no would_refuse;
//   c. a request claiming the lane without its token is let through and
//      counted `would_refuse` in the default report mode, and refused 403
//      `worker_identity_refused` after a restart with AMUX_WORKER_IDENTITY=enforce,
//      while the lane's own CLI call still succeeds;
//   d. AMUX_WORKER_IDENTITY=off takes the check away again (the off switch).
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/worker-identity-default.mjs
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', FAKE_CLAUDE_ENV_KEYS: 'AMUX_WORKER_TOKEN',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const identity = async () => (await amux.req('GET', '/api/contract/counters')).body.worker_identity || {};
const token = lane => amux.tmux('show-environment', '-t', 'amux-' + lane, 'AMUX_WORKER_TOKEN').trim().split('=').slice(1).join('=');
// The shipped CLI, with exactly the environment a lane gives it.
const cli = (lane, tok, ...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, AMUX_SESSION: lane, AMUX_WORKER_TOKEN: tok},
  encoding: 'utf8', timeout: 30000,
});
const forged = (lane, title) => amux.req('POST', '/api/board',
  {title, session: lane, type: 'chore', status: 'backlog'}, 10000, {'X-Amux-Session': lane});

const lanes = {'id-claude': 'claude', 'id-codex': 'codex', 'id-gemini': 'gemini'};
let ok = false;
try {
  for (const [name, provider] of Object.entries(lanes)) {
    const dir = path.join(amux.root, name);
    fs.mkdirSync(dir);
    const created = await amux.req('POST', '/api/sessions', {name, dir, start: false, provider});
    check(`worker created with no contract settings: ${name}`, created.status === 201, created);
    const env = fs.readFileSync(path.join(amux.home, 'sessions', name + '.env'), 'utf8');
    check(`${name} is not on the contract`, !/AMUX_CONTRACT_DONE=1/.test(env), env);
    // One board switch on (a created worker has none), so the lane has a board
    // for its CLI to write to; nothing about identity changes.
    fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'), '\nAMUX_BOARD_FORCE_ADHERENCE=1\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n');
    const started = await amux.req('POST', `/api/sessions/${name}/start`);
    check(`${name} started`, started.status < 300, started);
    const mine = x => x.event === 'launch' && x.bin === provider
      && ((x.argv || []).join(' ').includes(name) || x.session === name || (x.cwd || '').endsWith('/' + name));
    const launch = await waitFor(`${provider} launch of ${name}`, () => amux.fakeLog().find(mine), 30000).catch(() => null);
    check(`a. ${provider} lane's launch environment carries AMUX_WORKER_TOKEN`, !!launch?.env_sha256?.AMUX_WORKER_TOKEN, amux.fakeLog().filter(x => x.bin === provider).slice(-2));
    check(`a. ${name} has a minted hash on disk`, fs.existsSync(path.join(amux.home, 'worker-tokens', name + '.sha256')));
  }
  const lane = 'id-claude';
  const tok = token(lane);
  check('the token is in the lane tmux session', /^[0-9a-f]{64}$/.test(tok), tok.length);

  // b. The CLI from inside the lane proves who it is.
  let before = await identity();
  check('report is the default mode', before.mode === 'report', before);
  const added = cli(lane, tok, 'board', 'add', 'identity e2e: added by the lane CLI');
  check('b. the lane CLI mutating call succeeded', /[A-Z]+-\d+/.test(added), added);
  let after = await identity();
  check('b. the CLI call was counted valid', after.requests.valid > before.requests.valid, {before, after});
  check('b. nothing the lane itself sent would be refused', after.requests.would_refuse === before.requests.would_refuse, after);

  // c. A tokenless claim: allowed and counted in report mode.
  const r1 = await forged(lane, 'identity e2e: forged in report mode');
  check('c. report mode lets a tokenless claim through', r1.status === 201, r1);
  after = await identity();
  check('c. report mode counted it as would_refuse', after.requests.would_refuse >= 1 && after.measured === true && after.n_considered >= 2, after);
  await waitFor('would_refuse line in the server log', () => fs.readFileSync(amux.serverLog, 'utf8').includes('worker_identity_would_refuse'), 10000);
  check('c. the server log names the lane once', fs.readFileSync(amux.serverLog, 'utf8').split('\n')
    .filter(l => l.includes('worker_identity_would_refuse') && l.includes(lane)).length === 1);
  check('c. the lane is listed as minted', (after.lanes.minted || []).includes(lane), after.lanes);

  // c. enforce mode: the same claim is refused, the lane's own CLI still works.
  await amux.down();
  amux.env.AMUX_WORKER_IDENTITY = 'enforce';
  await amux.up();
  check('restarted in enforce mode', (await identity()).mode === 'enforce');
  const r2 = await forged(lane, 'identity e2e: forged in enforce mode');
  check('c. enforce mode refuses a tokenless claim 403', r2.status === 403 && r2.body.code === 'worker_identity_refused', r2);
  const r3 = await amux.req('POST', '/api/board', {title: 'identity e2e: wrong token', session: lane, type: 'chore', status: 'backlog'},
    10000, {'X-Amux-Session': lane, 'X-Amux-Worker-Token': token('id-codex')});
  check("c. enforce mode refuses another lane's token", r3.status === 403, r3);
  const again = cli(lane, tok, 'board', 'add', 'identity e2e: lane CLI under enforce');
  check('c. the lane CLI is still accepted under enforce', /[A-Z]+-\d+/.test(again), again);
  const reads = await amux.req('GET', '/api/board', undefined, 10000, {'X-Amux-Session': lane});
  check('c. reads are not identity claims', reads.status === 200, reads.status);

  // d. off switch.
  await amux.down();
  amux.env.AMUX_WORKER_IDENTITY = 'off';
  await amux.up();
  const r4 = await forged(lane, 'identity e2e: forged with the switch off');
  check('d. AMUX_WORKER_IDENTITY=off checks nothing', r4.status === 201 && (await identity()).mode === 'off', r4);
  ok = true;
} finally {
  const receipt = {scenario: 'worker-identity-default', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(c => c.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
