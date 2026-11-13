#!/usr/bin/env node
// END-TO-END (governance PR 16): each worker's agent CLI gets its own sandbox
// settings, written from the worker's capabilities. A private amux (own home,
// port, tmux socket, fake CLIs):
//   a. report (default): a launched worker gets a settings layer with the
//      sandbox DISABLED, and the CLI is not given it;
//   b. the sandbox view names each rule's source: another worker's folder is a
//      write boundary, the owner token and other workers' env files are denied,
//      the network is unrestricted, and the runtime is measured;
//   c. a worker may read its own view, not another's;
//   d. the owner's AMUX_CAP_FOLDERS grant lifts the boundary and
//      AMUX_CAP_NETWORK narrows the network, each with its layer;
//   e. enforce: a relaunch passes `--settings <file>` to the CLI, and the file
//      enables the sandbox, fails closed without a runtime, and denies the
//      secrets to the Read and Edit tools too; counters count it;
//   f. off: a relaunch writes and passes nothing.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/cli-sandbox.mjs
import fs from 'node:fs';
import path from 'node:path';
import {startAmux, waitFor} from './harness.mjs';

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
const tenv = (lane, key) => {
  let out = '';
  try { out = amux.tmux('show-environment', '-t', 'amux-' + lane, key).trim(); } catch { return null; }
  return out.startsWith(key + '=') ? out.slice(key.length + 1) : null;
};
const dirOf = name => path.join(amux.root, 'customers', name);
const mkWorker = async name => {
  fs.mkdirSync(dirOf(name), {recursive: true});
  const r = await amux.req('POST', '/api/sessions', {name, dir: dirOf(name), start: false});
  check('worker created ' + name, r.status === 201, r);
  fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'),
    '\nCC_TAGS=customers\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n');
};
const launches = name => amux.fakeLog().filter(x => x.event === 'launch' && (x.argv || []).includes(name));
const start = async name => {
  const before = launches(name).length;
  const s = await amux.req('POST', `/api/sessions/${name}/start`);
  check('worker started ' + name, s.status < 300, s);
  await waitFor('provider launch ' + name, () => launches(name).length > before, 30000);
  return launches(name).at(-1).argv || [];
};
const restartWorker = async name => {
  await amux.req('POST', `/api/sessions/${name}/stop`);
  await waitFor(name + ' stopped', async () => !(await amux.req('GET', `/api/sessions/${name}`)).body.running, 30000).catch(() => {});
  return start(name);
};
const settingsFile = name => path.join(amux.home, 'sandbox', name + '.claude-settings.json');
const settings = name => JSON.parse(fs.readFileSync(settingsFile(name), 'utf8'));
const OWNER = () => ({Authorization: 'Bearer ' + fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim()});
const view = name => amux.req('GET', `/api/workers/${name}/sandbox`, undefined, 10000, OWNER());

let ok = false;
try {
  await mkWorker('sb-a');
  await mkWorker('sb-b');

  // a. Report mode.
  const argvA = await start('sb-a');
  check('a. report: the settings layer is written with the sandbox disabled', fs.existsSync(settingsFile('sb-a'))
    && settings('sb-a').sandbox.enabled === false, fs.existsSync(settingsFile('sb-a')) && settings('sb-a'));
  check('a. report: the CLI is not given the layer', !argvA.includes('--settings'), argvA);

  // b. The view names sources.
  const v = (await view('sb-a')).body;
  const entry = (rule, needle) => (v.entries || []).find(e => e.rule === rule && e.path.endsWith(needle));
  check('b. view: report mode, runtime measured', v.mode === 'report' && v.status === 'report_only'
    && v.runtime?.measured === true && v.measured === true && v.n_considered > 0, v);
  check("b. another worker's folder is a write boundary, named", entry('denyWrite', 'customers/sb-b')?.source.includes('sb-b'), v.entries);
  check('b. the owner token and the other env file are denied', entry('denyRead', 'auth_token')
    && entry('denyRead', 'sessions/sb-b.env') && !entry('denyRead', 'sessions/sb-a.env'), v.entries);
  check('b. the network is unrestricted by default', JSON.stringify(v.network?.allowed_domains) === '["*"]', v.network);

  // c. Own view only.
  const tokA = tenv('sb-a', 'AMUX_WORKER_TOKEN');
  await start('sb-b');
  const tokB = tenv('sb-b', 'AMUX_WORKER_TOKEN');
  check('c. both workers hold a token', !!tokA && !!tokB, {tokA: !!tokA, tokB: !!tokB});
  const own = await amux.req('GET', '/api/workers/sb-a/sandbox', undefined,
    30000, {'X-Amux-Session': 'sb-a', 'X-Amux-Worker-Token': tokA});
  const other = await amux.req('GET', '/api/workers/sb-a/sandbox', undefined,
    30000, {'X-Amux-Session': 'sb-b', 'X-Amux-Worker-Token': tokB});
  check("c. a worker reads its own view and not another's", own.status === 200 && other.status === 403, {own: own.status, other: other.status});

  // d. Owner grants.
  fs.appendFileSync(path.join(amux.home, 'sessions', 'sb-a.env'),
    `AMUX_CAP_FOLDERS=${dirOf('sb-b')}\nAMUX_CAP_NETWORK=api.github.com,*.amux.io\n`);
  const v2 = (await view('sb-a')).body;
  check("d. a folder grant lifts the boundary", !(v2.entries || []).some(e => e.rule === 'denyWrite' && e.path.endsWith('customers/sb-b'))
    && (v2.entries || []).some(e => e.rule === 'allowWrite' && e.path.endsWith('customers/sb-b') && e.source.includes('AMUX_CAP_FOLDERS')), v2.entries);
  check('d. the owner narrows the network, with its layer', JSON.stringify(v2.network.allowed_domains) === '["api.github.com","*.amux.io"]'
    && v2.network.source.includes('worker sb-a'), v2.network);

  // e. Enforce.
  await amux.down();
  amux.env.AMUX_CLI_SANDBOX = 'enforce';
  await amux.up();
  const argvE = await restartWorker('sb-b');
  const i = argvE.indexOf('--settings');
  check('e. enforce: the CLI is given the layer', i >= 0 && argvE[i + 1] === settingsFile('sb-b'), argvE);
  const sE = settings('sb-b');
  check('e. the layer enables the sandbox and fails closed', sE.sandbox.enabled === true && sE.sandbox.failIfUnavailable === true, sE.sandbox);
  check('e. the Read and Edit tools are denied the secrets and the boundary', (sE.permissions?.deny || []).some(d => d.startsWith('Read(/') && d.includes('auth_token'))
    && (sE.permissions?.deny || []).some(d => d.startsWith('Edit(/') && d.includes('customers/sb-a')), sE.permissions);
  const k = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.cli_sandbox || {};
  check('e. counters: enforce mode, runtime measured, a launch counted', k.mode === 'enforce' && k.measured === true
    && ((k.counts?.enforced || 0) + (k.counts?.enforced_runtime_unavailable || 0)) >= 1 && Array.isArray(k.runtime?.missing), k);
  const vE = (await view('sb-b')).body;
  check('e. the view says enforced, or unavailable when the runtime is missing', vE.runtime.available
    ? vE.status === 'enforced' : vE.status === 'unavailable', {status: vE.status, runtime: vE.runtime});

  // f. Off.
  await amux.down();
  amux.env.AMUX_CLI_SANDBOX = 'off';
  await amux.up();
  fs.rmSync(settingsFile('sb-a'), {force: true});
  const argvO = await restartWorker('sb-a');
  check('f. off: nothing written, nothing passed', !argvO.includes('--settings') && !fs.existsSync(settingsFile('sb-a')), argvO);
  ok = true;
} finally {
  const receipt = {scenario: 'cli-sandbox', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
