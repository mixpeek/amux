#!/usr/bin/env node
// END-TO-END (governance PR 17): a worker in a customer boundary group runs its
// agent CLI inside a mount namespace holding its group's folders and nothing
// else of the home. A private amux (own home, port, tmux socket, fake CLIs);
// the fake CLI runs a probe where the agent runs and logs what it saw:
//   a. report (default): the plan is recorded and shown, nothing is wrapped
//      (the agent still sees the other customer's folder);
//   b. enforce: the boundary worker writes its own folder, cannot see the other
//      customer's folder or the owner token, and still reaches the amux API
//      with its worker token; counters count the wrap;
//   c. enforce: a worker outside any boundary is untouched;
//   d. off: nothing is wrapped.
// Where bwrap cannot start a namespace (some CI runners), the enforce checks
// are recorded as skipped with the measured reason, never passed.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/customer-isolation.mjs
import fs from 'node:fs';
import path from 'node:path';
import {startAmux, waitFor} from './harness.mjs';

const TOKEN_FILE = 'auth' + '_token';
const probe = [
  'printf "W=%s\\n" "$(touch "$PROBE_OWN/probe-ok" 2>/dev/null && echo ok || echo no)"',
  'printf "B=%s\\n" "$(test -e "$PROBE_OTHER" && echo visible || echo absent)"',
  'printf "T=%s\\n" "$(test -e "$PROBE_TOKEN" && echo visible || echo absent)"',
  'printf "API=%s\\n" "$(curl -sk -o /dev/null -w "%{http_code}" -H "X-Amux-Session: $AMUX_SESSION" -H "X-Amux-Worker-Token: $AMUX_WORKER_TOKEN" "$AMUX_URL/api/workers/$AMUX_SESSION/isolation")"',
].join('; ');

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_ISOLATED: '0', AMUX_RS_SCHEDULER: '0', AMUX_BOARD_DRIVE_SECS: '0',
  AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0', AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
  FAKE_CLAUDE_RAW_LOG: '', FAKE_CLAUDE_PROBE_SH: probe,
}});
const checks = [];
const skipped = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const dirOf = name => path.join(amux.root, 'customers', name);
const logFile = path.join(amux.root, 'fake-claude.log');
fs.writeFileSync(logFile, '');
const mkWorker = async (name, group) => {
  fs.mkdirSync(dirOf(name), {recursive: true});
  const r = await amux.req('POST', '/api/sessions', {name, dir: dirOf(name), start: false});
  check('worker created ' + name, r.status === 201, r);
  const other = name === 'iso-a' ? dirOf('iso-b') : dirOf('iso-a');
  // The fake CLI's log is the test's ground truth, so the owner grants the
  // worker that one file; the probe paths ride the worker's own env file.
  fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'),
    `\nCC_TAGS=${group}\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nAMUX_CAP_FOLDERS=${logFile}\n` +
    `PROBE_OWN=${dirOf(name)}\nPROBE_OTHER=${other}\nPROBE_TOKEN=${path.join(amux.home, TOKEN_FILE)}\n`);
};
const launches = name => amux.fakeLog().filter(x => x.event === 'launch' && x.cwd === dirOf(name));
const start = async name => {
  const before = launches(name).length;
  const s = await amux.req('POST', `/api/sessions/${name}/start`);
  if (s.status >= 300) return {refused: s};
  await waitFor('provider launch ' + name, () => launches(name).length > before, 45000);
  return launches(name).at(-1);
};
const restartWorker = async name => {
  await amux.req('POST', `/api/sessions/${name}/stop`);
  await waitFor(name + ' stopped', async () => !(await amux.req('GET', `/api/sessions/${name}`)).body.running, 30000).catch(() => {});
  return start(name);
};
const seen = rec => Object.fromEntries((rec?.probe?.out || '').trim().split('\n').filter(Boolean).map(l => l.split('=')));
const OWNER = () => ({Authorization: 'Bearer ' + fs.readFileSync(path.join(amux.home, TOKEN_FILE), 'utf8').trim()});
const view = name => amux.req('GET', `/api/workers/${name}/isolation`, undefined, 15000, OWNER());

let ok = false;
try {
  fs.mkdirSync(path.join(amux.home, 'env'), {recursive: true});
  fs.writeFileSync(path.join(amux.home, 'env', 'tenant-a.env'), 'AMUX_CAP_ISOLATION=boundary\n');
  fs.writeFileSync(path.join(amux.home, 'env', 'tenant-b.env'), 'AMUX_CAP_ISOLATION=boundary\n');
  await mkWorker('iso-a', 'tenant-a');
  await mkWorker('iso-b', 'tenant-b');
  await mkWorker('iso-plain', 'misc');

  // a. Report mode.
  const r1 = await start('iso-a');
  const s1 = seen(r1);
  check('a. report: the agent is not wrapped (it still sees the other customer)', s1.B === 'visible' && s1.W === 'ok', r1?.probe);
  const v1 = (await view('iso-a')).body;
  check('a. report: the plan is shown with its boundary and runner, measured', v1.mode === 'report' && v1.status === 'report_only'
    && v1.boundary?.group === 'tenant-a' && v1.runner?.measured === true && v1.measured === true && v1.n_considered > 0, v1);
  check("a. the plan mounts the worker's folder and not the other customer's", (v1.mounts || []).some(m => m.kind === 'rw' && m.path === dirOf('iso-a'))
    && !(v1.mounts || []).some(m => m.path === dirOf('iso-b')), v1.mounts);
  check('a. the last launch was recorded as reported', v1.last_launch?.status === 'reported', v1.last_launch);
  const vp = (await view('iso-plain')).body;
  check('a. a worker outside any boundary has no plan', vp.status === 'not_boundary' && (vp.mounts || []).length === 0, vp);

  // b. Enforce.
  await amux.down();
  amux.env.AMUX_CUSTOMER_ISOLATION = 'enforce';
  await amux.up();
  const vE = (await view('iso-a')).body;
  if (!vE.runner?.available) {
    // No namespace runner here: the start must be REFUSED, never run unwrapped.
    const r = await restartWorker('iso-a');
    check('b. no runner: the boundary worker is refused, not run unwrapped', r.refused && vE.status === 'refused_no_runner', {r, vE});
    skipped.push({name: 'b. enforce wrap', reason: vE.runner?.reason || 'runner unavailable'});
    console.log('skip - b. enforce wrap:', vE.runner?.reason);
  } else {
    const r2 = await restartWorker('iso-a');
    const s2 = seen(r2);
    check('b. enforce: the agent writes its own folder', s2.W === 'ok' && fs.existsSync(path.join(dirOf('iso-a'), 'probe-ok')), r2?.probe);
    check("b. enforce: the other customer's folder is absent", s2.B === 'absent', r2?.probe);
    check('b. enforce: the owner token is absent', s2.T === 'absent', r2?.probe);
    check('b. enforce: the agent reaches the amux API with its worker token', s2.API === '200', r2?.probe);
    const k = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.customer_isolation || {};
    check('b. counters: enforce mode, a wrap counted', k.mode === 'enforce' && k.measured === true && (k.counts?.wrapped || 0) >= 1, k);
    const vE2 = (await view('iso-a')).body;
    check('b. the view says enforced and the last launch was wrapped', vE2.status === 'enforced' && vE2.last_launch?.status === 'wrapped', vE2);
  }

  // c. Enforce leaves a worker outside any boundary alone.
  const r3 = await start('iso-plain');
  check('c. enforce: a worker outside any boundary is untouched', seen(r3).B === 'visible', r3?.probe);

  // d. Off.
  await amux.down();
  amux.env.AMUX_CUSTOMER_ISOLATION = 'off';
  await amux.up();
  const r4 = await restartWorker('iso-a');
  check('d. off: nothing is wrapped', seen(r4).B === 'visible', r4?.probe);
  ok = true;
} finally {
  const receipt = {scenario: 'customer-isolation', ok, checks, skipped};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, skipped, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
