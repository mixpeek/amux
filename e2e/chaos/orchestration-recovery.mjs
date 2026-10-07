#!/usr/bin/env node
// Real HTTPS server + durable DB + SIGKILL/restart. No production workers,
// external service calls, or model spend. Unit tests cover claim races/reserve
// verdicts; this proves the installed paths, persisted state and socket gates.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { startAmux, waitFor } from './harness.mjs';
const checks = [];
const check = (name, ok, detail) => { checks.push({ name, ok: !!ok, detail }); if (!ok) throw new Error(name + ': ' + JSON.stringify(detail)); };
const quote = s => "'" + s.replaceAll("'", "'\\''") + "'";
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY, env: { AMUX_RS_SCHEDULER: '1', AMUX_ISOLATED: '0', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0', AMUX_MODEL_CATALOG_REFRESH_SECS: '0', ANTHROPIC_API_KEY: '', OPENAI_API_KEY: '', GEMINI_API_KEY: '', GOOGLE_API_KEY: '' } });
const runs = async id => { const r = await amux.req('GET', `/api/schedules/runs?schedule_id=${id}&limit=50`); return Array.isArray(r.body) ? r.body : r.body.runs || r.body.rows || []; };
const run = async id => { const r = await amux.req('POST', `/api/schedules/${id}/run`, {}); check('run accepted ' + id, r.status < 300, r.body); return waitFor('recorded shell result', async () => (await runs(id)).find(r => ['ok', 'error'].includes(r.status)), 20000); };
try {
  const health = (await amux.req('GET', '/health')).body;
  for (const lane of ['hub', 'lane-a', 'lane-b', 'foreign', 'isolated']) {
    const r = await amux.req('POST', '/api/sessions', { name: lane, dir: amux.root, start: false });
    check('stopped worker configured ' + lane, r.status === 201, r.body);
    fs.appendFileSync(path.join(amux.home, 'sessions', lane + '.env'), `\nAMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=${lane === 'foreign' ? 'foreign' : 'hub'}\n${lane === 'isolated' ? 'CC_ISOLATED=1\n' : 'CC_ISOLATED=0\n'}`);
  }
  const root = await amux.req('POST', '/api/board', { title: 'Root dependency', type: 'chore', status: 'backlog', session: 'hub' });
  check('root dependency created', root.status < 300 && root.body.id, root.body);
  const parents = [];
  for (let i = 0; i < 12; i++) {
    const p = await amux.req('POST', '/api/board', { title: `GS12 proof fixture ${i}`, type: 'ops', status: 'backlog', session: 'hub', depends_on: [root.body.id] });
    check('dependent created ' + i, p.status < 300 && p.body.id, p.body); parents.push(p.body.id);
  }
  const moved = await amux.req('PATCH', `/api/board/${root.body.id}`, { session: 'lane-a' });
  check('one executor moves without moving its dependency component', moved.status < 300, moved.body);
  for (const lane of ['foreign', 'isolated']) {
    const refused = await amux.req('PATCH', `/api/board/${root.body.id}`, { session: lane });
    check('ownership boundary preserved ' + lane, refused.status === 409, refused.body);
  }
  const scratch = path.join(amux.root, 'transient proof.py');
  fs.writeFileSync(scratch, 'print("durable-script-executed")\n');
  const schedule = await amux.req('POST', '/api/schedules', { title: 'Durable proof', session: 'lane-a', kind: 'shell', command: 'python3 {script}', script_path: scratch, schedule_expr: 'daily at 3am', enabled: 0 });
  check('schedule bound to durable script', schedule.status === 201 && schedule.body.command.includes('schedule-artifacts'), schedule.body);
  fs.unlinkSync(scratch);
  await amux.down();
  await amux.up();
  const restarted = (await amux.req('GET', '/health')).body;
  check('different process, identical binary after abrupt restart', health.pid !== restarted.pid && health.build === restarted.build, { before: health.pid, after: restarted.pid });
  for (const id of parents) {
    const p = (await amux.req('GET', `/api/board/${id}`)).body;
    check('dependency preserved across restart ' + id, p.session === 'hub' && p.depends_on.includes(root.body.id), p);
  }
  const result = await run(schedule.body.id);
  check('deleted scratch script executes after restart', result.status === 'ok' && JSON.stringify(result).includes('durable-script-executed'), result);
  const pipeline = await amux.req('POST', '/api/schedules', { title: 'Failed upstream', kind: 'shell', command: '(echo failed-proof; exit 37) | tail -1', schedule_expr: 'daily at 3am', enabled: 0 });
  check('pipeline fixture created', pipeline.status === 201, pipeline.body);
  const failure = await run(pipeline.body.id);
  check('upstream failure reaches persisted run status and exit code', failure.status === 'error' && failure.exit_code === 37, failure);
  const missing = await amux.req('POST', '/api/schedules', { title: 'Missing source', kind: 'shell', command: 'python3 {script}', script_path: scratch, schedule_expr: 'daily at 3am' });
  check('missing artifact rejected before schedule exists', missing.status === 400 && missing.body.code === 'schedule_script_invalid', missing.body);
  const never = path.join(amux.root, 'must-not-replay-effect');
  const interrupted = await amux.req('POST', '/api/schedules', { title: 'Interrupted execution', kind: 'shell', command: `echo started; sleep 30; echo unsafe-replay > ${quote(never)}`, schedule_expr: 'daily at 3am', enabled: 0 });
  check('crash fixture created', interrupted.status === 201, interrupted.body);
  const begun = await amux.req('POST', `/api/schedules/${interrupted.body.id}/run`, {});
  check('long job accepted', begun.status < 300, begun.body);
  await waitFor('in-flight run persisted', async () => (await runs(interrupted.body.id)).some(r => r.status === 'running'), 15000);
  await amux.down(); await amux.up();
  const orphan = await waitFor('in-flight result reconciled', async () => (await runs(interrupted.body.id)).find(r => r.status === 'error'), 20000);
  check('crashed execution stays explicitly unmeasured', /restarted/.test(orphan.note), orphan);
  check('uncertain side effect not blindly replayed', !fs.existsSync(never) && (await runs(interrupted.body.id)).length === 1);

  await waitFor('installed bundled observer', () => fs.existsSync(path.join(amux.home, 'native-status.py')), 20000);
  check('installed observer transmits minted identity', fs.readFileSync(path.join(amux.home, 'native-status.py'), 'utf8').includes('X-Amux-Worker-Token'));
  // A real schedule run mints an identity. Its installed observer delivers the
  // spool over HTTPS, so this catches an installed-hook/source mismatch.
  const folder = path.join(amux.home, 'status-events', 'lane-a', 'deadbeef'); fs.mkdirSync(folder, { recursive: true });
  fs.writeFileSync(path.join(folder, '..', 'current.json'), JSON.stringify({ run_id: 'deadbeef', provider: 'claude', started: Date.now()/1000-10 }));
  for (const [sequence, state, event] of [[1, 'active', 'UserPromptSubmit'], [2, 'idle', 'Stop']]) {
    fs.writeFileSync(path.join(folder, String(sequence).padStart(20, '0') + '.json'), JSON.stringify({ native_status: true, provider: 'claude', run_id: 'deadbeef', sequence, event_ts: Date.now()/1000, state, event }));
  }
  const observer = await amux.req('POST', '/api/schedules', { title: 'Installed observer delivery', session: 'lane-a', kind: 'shell', command: `AMUX_STATUS_HOME=${quote(amux.home)} AMUX_STATUS_WORKER=lane-a AMUX_STATUS_RUN_ID=deadbeef AMUX_STATUS_URL=${quote(amux.base)} python3 ${quote(path.join(amux.home, 'native-status.py'))} --deliver`, schedule_expr: 'daily at 3am', enabled: 0 });
  check('observer schedule created', observer.status === 201, observer.body);
  const observed = await run(observer.body.id);
  check('installed observer delivery succeeds with real run identity', observed.status === 'ok', observed);
  check('both lifecycle edges acknowledged from spool', !fs.existsSync(path.join(folder, '00000000000000000001.json')) && !fs.existsSync(path.join(folder, '00000000000000000002.json')));
  const db = path.join(amux.home, 'amux.db');
  const count = () => Number(execFileSync('python3', ['-c', 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute("SELECT COUNT(*) FROM session_events WHERE type=\'session.native_status\' AND json_extract(data,\'$.run_id\')=\'deadbeef\'").fetchone()[0])', db], { encoding: 'utf8' }).trim());
  // If the schema stores a projection instead of raw edges this assertion
  // should fail; a spool unlink alone is not proof of durable recovery.
  check('ordered lifecycle edges persisted exactly once', count() === 2, { count: count() });
  await amux.down(); await amux.up();
  check('lifecycle observations survive another abrupt restart', count() === 2, { count: count() });
  check('successful durable job not spontaneously replayed on restart', (await runs(schedule.body.id)).filter(r => r.status === 'ok').length === 1);
} catch (e) { checks.push({ name: 'scenario completed', ok: false, detail: String(e.stack || e) }); }
finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
const receipt = { measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks };
fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
console.log(JSON.stringify(receipt, null, 2));
console.log('VERDICT:', !failed.length ? 'PASS' : 'FAIL');
process.exit(failed.length ? 1 : 0);
